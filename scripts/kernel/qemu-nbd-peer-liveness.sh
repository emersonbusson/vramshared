#!/usr/bin/env bash
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/kernel/qemu-nbd-peer-liveness.sh
# qemu-nbd-peer-liveness.sh — the SPEC #18 isolated peer-liveness campaign.
#
# WHY QEMU: an NBD peer that hangs leaves a swap read uninterruptible for
# ~150 s. On the daily host that is the 2026-10-07 incident shape;
# in a VM any stall is contained and the host stays intact. The peer here is
# the product's own daemon in its RAM-broker mode (`--backend ram --slices`),
# which needs no GPU and no sealed origin, so the campaign exercises the
# product binary in an isolated environment. Scenarios s13 to s15 use two
# disposable laboratory peers sharing a file; it is not a product test.
#
# One scenario per boot. A swap read into a dead or stopped peer can sit in
# D-state for minutes and userspace cannot kill it — a single boot running
# several scenarios would hang at the first stuck teardown and the later
# scenarios would never run. Each boot selects its scenario on the kernel
# cmdline (`ktest.scenario=sN`), reports its KTEST lines, and the host
# collects them per boot.
#
# Scenarios:
#   s1 peer live + swap pressure      — swap I/O through the peer works
#   s2 peer crash (SIGKILL)           — kernel -EPIPE, EIO, bounded swapoff,
#                                       and a process touching its swapped
#                                       pages (the SIGBUS shape)
#   s3 peer hang (SIGSTOP)            — the read sticks; SIGCONT recovers
#   s4 daemon stop (SIGTERM)          — graceful stop, kernel behavior
#   s5 reconnect and restart          — fresh connect + swapon works
#   s6 serving-rate leg (opt-in)      — the product's origin path in a
#                                       guest, served by the serve pool
#                                       (SPEC wsl2-nbd-serving-throughput,
#                                       ITEM-4): tier vs fallback 4 KiB
#                                       rates in one run, p50 at N=1/N=4.
#                                       Needs KVM, three virtio disks and
#                                       the switch_root flow below; without
#                                       /dev/kvm the leg reports
#                                       s6_skipped=no_kvm instead of
#                                       measuring QEMU's emulation.
#
#   s12 sealed origin integrity       — native startup refusals, byte-exact
#                                       NBD/origin I/O and owned shutdown;
#                                       functional leg, TCG permitted
#   s13 dual peer, occupied swap     — kill one laboratory peer with a read
#                                       in flight and verify all hog bytes
#   s14 dual peer, occupied swap     — interrupt a completed but unacknowledged
#                                       write and verify the data after retry
#   s15 dual peer, occupied swap     — kill after half a write, verify exact
#                                       replay through the surviving peer
#   s16 temporary file swap          — exercise occupied-tier drain with an
#                                       independently backed ext4 swap file
#   s20 live replacement peer — reattach without removing active swap
#   s19 independent product peers — shared origin, isolated mount namespaces
#   s18 product emergency lifecycle — real creator and native cleanup
#   s17 tracked page migration       — retain memory pressure and identify
#                                       pages that reach the temporary swap
#
# Measured boundary and mechanism: the guest kernel keeps clean swapped
# pages in the swap cache while RAM allows, so teardowns of a cached tier
# read nothing. The hog runs inside a `memory.max` cgroup whose reclaim
# evicts the swap-cache copies (SwapCached ~31 MiB -> under 400 KiB), so
# the swap device genuinely holds the pages. The limit is lifted before
# each teardown: a page cannot be faulted back into an over-limit cgroup
# (swapoff then fails with ENOMEM). The teardown result is bimodal — fast
# (3-4 s) when no swap-out write is in flight at peer death, stalled
# 90-153 s+ on the kernel's stuck-request path when one is; the product's
# 120 s SWAPOFF_TIMEOUT converts the stall into "backend preserved".
#
# usage: qemu-nbd-peer-liveness.sh [bzImage] [ramsharedd] [nbd-client] [modules_dir]
set -euo pipefail

BZ="${1:-/lib/modules/6.18.40.1-microsoft-standard-WSL2+.build2.bak/build/arch/x86/boot/bzImage}"
DAEMON="${2:-/opt/ramshared/current/bin/ramsharedd}"
NBD_CLIENT="${3:-/usr/sbin/nbd-client}"
MODS="${4:-/lib/modules/6.18.40.1-microsoft-standard-WSL2+.build2.bak/kernel/drivers/block}"
# Optional native released-peer cleanup extension for the functional s12 leg.
RECOVERY_CLI=${KTEST_RECOVERY_CLI:-}
EMERGENCY_TEST=${KTEST_EMERGENCY_TEST:-}
SCENARIOS=${KTEST_SCENARIOS:-s1 s2 s3 s4 s5}
[[ "$SCENARIOS" =~ [^[:space:]] ]] || { echo "no scenario selected" >&2; exit 2; }
for scenario in $SCENARIOS; do
  case "$scenario" in
    s1|s2|s3|s4|s5|s6|s7|s9|s10|s11|s12|s13|s14|s15|s16|s17|s18|s19|s20) ;;
    *) echo "unsupported scenario: $scenario" >&2; exit 2 ;;
  esac
done
if [ -n "$RECOVERY_CLI" ]; then
  case " $SCENARIOS " in *" s12 "*) ;; *) echo "recovery CLI requires s12" >&2; exit 2 ;; esac
  [ -x "$RECOVERY_CLI" ] || { echo "missing recovery CLI: $RECOVERY_CLI" >&2; exit 2; }
fi

case " $SCENARIOS " in *" s18 "*) [ -x "$EMERGENCY_TEST" ] || { echo "s18 requires KTEST_EMERGENCY_TEST" >&2; exit 2; } ;; esac

for f in "$BZ" "$DAEMON" "$NBD_CLIENT" "$MODS/nbd.ko" "$MODS/zram/zram.ko"; do
  [ -f "$f" ] || { echo "missing file: $f" >&2; exit 2; }
done
command -v qemu-system-x86_64 >/dev/null || { echo "qemu-system-x86_64 missing" >&2; exit 2; }
command -v gcc >/dev/null || { echo "gcc missing (helpers build from source)" >&2; exit 2; }
[ -x /bin/busybox ] || { echo "busybox-static missing" >&2; exit 2; }

WORK="$(mktemp -d)"; trap 'rm -rf "$WORK"' EXIT
IRD="$WORK/irfs"; mkdir -p "$IRD/bin" "$IRD/modules"
cp /bin/busybox "$IRD/bin/busybox"
cp "$DAEMON" "$IRD/ramsharedd"
cp "$NBD_CLIENT" "$IRD/nbd-client"
EMERGENCY_TOOLS=()
if [ -n "$EMERGENCY_TEST" ]; then
  cp "$EMERGENCY_TEST" "$IRD/emergency-test"
  for tool in fallocate mkswap swapon; do
    tool_path=$(command -v "$tool")
    EMERGENCY_TOOLS+=("$tool_path")
    mkdir -p "$IRD$(dirname "$tool_path")"
    cp "$tool_path" "$IRD$tool_path"
  done
fi
if [ -n "$RECOVERY_CLI" ]; then
  cp "$RECOVERY_CLI" "$IRD/ramshared"
  sha256sum "$RECOVERY_CLI" | awk '{print $1}' > "$IRD/expected-cli.sha256"
fi
cp "$MODS/nbd.ko" "$IRD/modules/"
cp "$MODS/zram/zram.ko" "$IRD/modules/"
# zram depends on zsmalloc; without it the insmod fails with an unknown
# symbol, so the s9 leg (and any zram use) carries it too.
[ -f "$MODS/../../mm/zsmalloc.ko" ] && cp "$MODS/../../mm/zsmalloc.ko" "$IRD/modules/" || echo "zsmalloc.ko not found under $MODS/../../mm" >&2
[ -f "$MODS/virtio_blk.ko" ] && cp "$MODS/virtio_blk.ko" "$IRD/modules/"
sha256sum "$DAEMON" | awk '{print $1}' > "$IRD/expected-ramsharedd.sha256"

# Anonymous-memory pressure with deterministic swap-out: allocate `total`
# MiB, then MADV_PAGEOUT `out` MiB of them. Reclaim-pressure-only variants
# either never swap (free memory stays high) or livelock the guest (the
# peer cannot serve the swap it is holding pages for). PAGEOUT makes the
# swap device carry real pages, so every teardown measurement has work.
cat > "$WORK/memhog.c" <<'C'
#define _GNU_SOURCE
#include <stdlib.h>
#include <string.h>
#include <stdio.h>
#include <unistd.h>
#include <sys/mman.h>
#include <signal.h>
static char **g_chunks;
static size_t g_mb;
static volatile sig_atomic_t g_touch = 0;
static volatile sig_atomic_t g_verify = 0;
static volatile sig_atomic_t g_dirty = 0;
static void on_usr1(int sig) { (void)sig; g_touch = 1; }
static void on_usr2(int sig) { (void)sig; g_verify = 1; }
static void on_alarm(int sig) { (void)sig; g_dirty = 1; }
int main(int argc, char **argv) {
    size_t mb = argc > 1 ? (size_t)atoi(argv[1]) : 64;
    size_t out_mb = argc > 2 ? (size_t)atoi(argv[2]) : 0;
    size_t post_mb = argc > 3 ? (size_t)atoi(argv[3]) : 0;
    size_t chunk = 1024 * 1024;
    char **chunks = calloc(mb, sizeof(char *));
    if (!chunks) return 2;
    for (size_t i = 0; i < mb; i++) {
        if (posix_memalign((void **)&chunks[i], 4096, chunk) != 0) {
            fprintf(stderr, "alloc failed at %zu MB\n", i);
            return 2;
        }
        unsigned long long *words = (unsigned long long *)chunks[i];
        for (size_t j = 0; j < chunk / sizeof(*words); j++)
            words[j] = 0x9e3779b97f4a7c15ULL ^ ((unsigned long long)i << 32) ^ j;
        usleep(1000);  /* let the cgroup's reclaim keep pace */
    }
    for (size_t i = 0; i < out_mb && i < mb; i++) {
        if (madvise(chunks[i], chunk, MADV_PAGEOUT) != 0)
            fprintf(stderr, "madvise pageout failed at %zu\n", i);
    }
    /* Fresh anonymous memory until MemFree drops under the reserve: the
     * clean swap-cache copies of the paged-out chunks are evicted only
     * under real pressure, and the reserve keeps the peer's own buffers
     * allocatable (a fully exhausted guest livelocks in reclaim). */
    long reserve_kb = argc > 3 ? atol(argv[3]) : 0;
    size_t post = 0;
    while (reserve_kb > 0) {
        FILE *f = fopen("/proc/meminfo", "r");
        long free_kb = -1;
        char line[256];
        while (f && fgets(line, sizeof line, f)) {
            if (sscanf(line, "MemFree: %ld kB", &free_kb) == 1) break;
        }
        if (f) fclose(f);
        if (free_kb >= 0 && free_kb < reserve_kb) break;
        char *p = malloc(chunk);
        if (!p) break;
        memset(p, 0x5a, chunk);
        usleep(1000);  /* let reclaim keep pace: a faster loop OOM-kills */
        post++;
    }
    g_chunks = chunks; g_mb = mb;
    signal(SIGUSR1, on_usr1);
    signal(SIGUSR2, on_usr2);
    signal(SIGALRM, on_alarm);
    fprintf(stderr, "allocated %zu MB, paged out %zu MB, post %zu MB, holding\n",
            mb, out_mb, post);
    for (;;) {
        pause();
        if (g_touch) {
            unsigned long sum = 0;
            for (size_t i = 0; i < g_mb; i++) sum += (unsigned char)chunks[i][0];
            fprintf(stderr, "touched %zu MB sum=%lu\n", g_mb, sum);
            g_touch = 0;
        }
        if (g_verify) {
            unsigned long long mismatches = 0;
            for (size_t i = 0; i < g_mb; i++) {
                volatile unsigned long long *words = (volatile unsigned long long *)chunks[i];
                for (size_t j = 0; j < chunk / sizeof(*words); j++) {
                    unsigned long long expected = 0x9e3779b97f4a7c15ULL ^ ((unsigned long long)i << 32) ^ j;
                    if (words[j] != expected) mismatches++;
                }
            }
            fprintf(stderr, "full_verify bytes=%zu mismatches=%llu\n", g_mb * chunk, mismatches);
            g_verify = 0;
        }
        if (g_dirty) {
            for (size_t i = 0; i < g_mb; i++) {
                volatile unsigned long long *words = (volatile unsigned long long *)chunks[i];
                for (size_t j = 0; j < chunk / sizeof(*words); j += 4096 / sizeof(*words))
                    words[j] = words[j];
            }
            fprintf(stderr, "dirty_pass bytes=%zu\n", g_mb * chunk);
            g_dirty = 0;
        }
    }
    return 0;
}
C
gcc -O1 -static -o "$IRD/memhog" "$WORK/memhog.c"

# Track virtual addresses without faulting the workload's pages into memory.
# This helper runs only inside the disposable guest, with its two owned swaps.
cat > "$WORK/pagetrack.c" <<'C'
#define _GNU_SOURCE
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static int entry(FILE *pagemap, uint64_t address, uint64_t *value) {
    uint64_t offset = (address / 4096) * sizeof(*value);
    if (offset > INT64_MAX) return 0;
    return pread(fileno(pagemap), value, sizeof(*value), (off_t)offset) == (ssize_t)sizeof(*value);
}
int main(int argc, char **argv) {
    if (argc != 4 || sysconf(_SC_PAGESIZE) != 4096) return 2;
    char path[128];
    if (snprintf(path, sizeof(path), "/proc/%s/pagemap", argv[2]) >= (int)sizeof(path)) return 2;
    FILE *pagemap = fopen(path, "rb");
    if (!pagemap) { perror("pagemap"); return 1; }
    if (!strcmp(argv[1], "snapshot")) {
        if (snprintf(path, sizeof(path), "/proc/%s/maps", argv[2]) >= (int)sizeof(path)) return 2;
        FILE *maps = fopen(path, "r"), *out = fopen(argv[3], "wb");
        if (!maps || !out) return 1;
        char *line = NULL; size_t cap = 0; uint64_t count = 0, other = 0, pages = 0;
        while (getline(&line, &cap, maps) > 0) {
            unsigned long long start, end; char permissions[5] = {0};
            if (sscanf(line, "%llx-%llx %4s", &start, &end, permissions) != 3 || permissions[0] != 'r') continue;
            if (start >= end || end > 0x800000000000ULL || end - start > (1ULL << 30)) continue;
            for (uint64_t address = start; address < end; address += 4096) {
                uint64_t value;
                if (++pages > 1000000 || !entry(pagemap, address, &value)) return 1;
                if (!(value & (1ULL << 62))) continue;
                if ((value & 31) != 0) { other++; continue; }
                if (fwrite(&address, sizeof(address), 1, out) != 1) return 1;
                count++;
            }
        }
        free(line); fclose(maps); fclose(out); fclose(pagemap);
        printf("tracked=%llu other=%llu\n", (unsigned long long)count, (unsigned long long)other);
        return count > 0 ? 0 : 1;
    }
    if (!strcmp(argv[1], "migrated")) {
        FILE *in = fopen(argv[3], "rb");
        if (!in) return 1;
        uint64_t address, count = 0, migrated = 0;
        while (fread(&address, sizeof(address), 1, in) == 1) {
            uint64_t value;
            if (!entry(pagemap, address, &value)) return 1;
            count++;
            if ((value & (1ULL << 62)) && (value & 31) != 0) migrated++;
        }
        if (ferror(in)) return 1;
        fclose(in); fclose(pagemap);
        printf("tracked=%llu migrated=%llu\n", (unsigned long long)count, (unsigned long long)migrated);
        return migrated > 0 ? 0 : 1;
    }
    return 2;
}
C
gcc -O2 -Wall -Wextra -Werror -static -o "$IRD/pagetrack" "$WORK/pagetrack.c"

# Disposable dual-process NBD laboratory peer for s13-s15. Both processes use
# one pre-opened file; this is a transport experiment, not a product service.
case " $SCENARIOS " in *" s13 "*|*" s14 "*|*" s15 "*)
cat > "$WORK/dualpeer.c" <<'C'
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <endian.h>
#include <errno.h>
#include <fcntl.h>
#include <linux/nbd.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/types.h>
#include <unistd.h>
#define SIZE_BYTES (256ULL * 1024 * 1024)
#define MAX_REQUEST (4U * 1024 * 1024)
struct dual_state {
    unsigned long long reads[2], writes[2], target_offset;
    unsigned int armed, paused, target_len, replay_verified;
    char expected[MAX_REQUEST];
};
static struct dual_state *state;
static int transfer(int fd, void *buf, size_t len, int writing) {
    size_t done = 0;
    while (done < len) {
        ssize_t n = writing ? write(fd, (char *)buf + done, len - done)
                            : read(fd, (char *)buf + done, len - done);
        if (n < 0 && errno == EINTR) continue;
        if (n <= 0) return -1;
        done += (size_t)n;
    }
    return 0;
}
static int file_transfer(int fd, void *buf, size_t len, uint64_t off, int writing) {
    size_t done = 0;
    while (done < len) {
        ssize_t n = writing ? pwrite(fd, (char *)buf + done, len - done, (off_t)(off + done))
                            : pread(fd, (char *)buf + done, len - done, (off_t)(off + done));
        if (n < 0 && errno == EINTR) continue;
        if (n <= 0) return -1;
        done += (size_t)n;
    }
    return 0;
}
static void serve(int socket_fd, int backing, int peer) {
    char *buf = malloc(MAX_REQUEST);
    if (!buf) _exit(2);
    for (;;) {
        struct nbd_request request;
        if (transfer(socket_fd, &request, sizeof(request), 0)) break;
        uint32_t type = ntohl(request.type), cmd = type & 0xffffU, len = ntohl(request.len);
        uint64_t offset = be64toh(request.from);
        if (ntohl(request.magic) != NBD_REQUEST_MAGIC || len > MAX_REQUEST ||
            offset > SIZE_BYTES || len > SIZE_BYTES - offset) break;
        if (cmd == NBD_CMD_DISC) break;
        int error = 0;
        if (cmd == NBD_CMD_WRITE) {
            if (transfer(socket_fd, buf, len, 0)) break;
            unsigned int mode = __atomic_load_n(&state->armed, __ATOMIC_ACQUIRE);
            if (peer == 0 && mode == 3 && len >= 2) {
                memcpy(state->expected, buf, len);
                state->target_offset = offset;
                state->target_len = len;
                if (file_transfer(backing, buf, len / 2, offset, 1)) error = EIO;
                if (!error) {
                    __atomic_store_n(&state->paused, 1, __ATOMIC_RELEASE);
                    while (__atomic_load_n(&state->armed, __ATOMIC_ACQUIRE)) usleep(1000);
                }
            }
            int replay = peer == 1 && mode == 3 &&
                __atomic_load_n(&state->paused, __ATOMIC_ACQUIRE) &&
                state->target_offset == offset && state->target_len == len &&
                !memcmp(state->expected, buf, len);
            if (!error && (file_transfer(backing, buf, len, offset, 1) ||
                ((type & NBD_CMD_FLAG_FUA) && fdatasync(backing)))) error = EIO;
            if (!error && replay) {
                char *verify = malloc(len);
                if (!verify || file_transfer(backing, verify, len, offset, 0) ||
                    memcmp(verify, state->expected, len)) error = EIO;
                else __atomic_store_n(&state->replay_verified, 1, __ATOMIC_RELEASE);
                free(verify);
            }
        } else if (cmd == NBD_CMD_READ) {
            if (file_transfer(backing, buf, len, offset, 0)) error = EIO;
        } else if (cmd == NBD_CMD_FLUSH) {
            if (fdatasync(backing)) error = EIO;
        } else error = EINVAL;
        unsigned int armed = __atomic_load_n(&state->armed, __ATOMIC_ACQUIRE);
        if (peer == 0 && !error && ((armed == 1 && cmd == NBD_CMD_READ) ||
                                    (armed == 2 && cmd == NBD_CMD_WRITE))) {
            __atomic_store_n(&state->paused, 1, __ATOMIC_RELEASE);
            while (__atomic_load_n(&state->armed, __ATOMIC_ACQUIRE)) usleep(1000);
        }
        struct nbd_reply reply = { .magic = htonl(NBD_REPLY_MAGIC), .error = htonl(error) };
        memcpy(reply.handle, request.handle, sizeof(reply.handle));
        if (transfer(socket_fd, &reply, sizeof(reply), 1)) break;
        if (!error && cmd == NBD_CMD_READ && transfer(socket_fd, buf, len, 1)) break;
        if (!error && cmd == NBD_CMD_READ) __atomic_add_fetch(&state->reads[peer], 1, __ATOMIC_RELAXED);
        if (!error && cmd == NBD_CMD_WRITE) __atomic_add_fetch(&state->writes[peer], 1, __ATOMIC_RELAXED);
    }
    free(buf);
    close(socket_fd);
    close(backing);
    _exit(0);
}
int main(int argc, char **argv) {
    int control = argc == 2 && (!strcmp(argv[1], "--stats") || !strcmp(argv[1], "--arm") || !strcmp(argv[1], "--arm-write") || !strcmp(argv[1], "--arm-partial-write"));
    int state_fd = open("/dual-state", control ? O_RDWR : O_CREAT|O_EXCL|O_RDWR, 0600);
    if (state_fd < 0 || (!control && ftruncate(state_fd, sizeof(*state)))) { perror("dual state"); return 2; }
    state = mmap(NULL, sizeof(*state), PROT_READ|PROT_WRITE, MAP_SHARED, state_fd, 0);
    close(state_fd);
    if (state == MAP_FAILED) { perror("dual mmap"); return 2; }
    if (control) {
        if (!strcmp(argv[1], "--arm")) __atomic_store_n(&state->armed, 1, __ATOMIC_RELEASE);
        else if (!strcmp(argv[1], "--arm-write")) __atomic_store_n(&state->armed, 2, __ATOMIC_RELEASE);
        else if (!strcmp(argv[1], "--arm-partial-write")) __atomic_store_n(&state->armed, 3, __ATOMIC_RELEASE);
        else printf("P0_READS=%llu\nP1_READS=%llu\nP0_WRITES=%llu\nP1_WRITES=%llu\nP0_PAUSED=%u\nP1_REPLAY_VERIFIED=%u\n",
            __atomic_load_n(&state->reads[0], __ATOMIC_ACQUIRE), __atomic_load_n(&state->reads[1], __ATOMIC_ACQUIRE),
            __atomic_load_n(&state->writes[0], __ATOMIC_ACQUIRE), __atomic_load_n(&state->writes[1], __ATOMIC_ACQUIRE),
            __atomic_load_n(&state->paused, __ATOMIC_ACQUIRE), __atomic_load_n(&state->replay_verified, __ATOMIC_ACQUIRE));
        return 0;
    }
    if (argc != 1) return 2;
    memset(state, 0, sizeof(*state));
    signal(SIGPIPE, SIG_IGN);
    signal(SIGCHLD, SIG_IGN);
    int nbd = open("/dev/nbd0", O_RDWR), file = open("/dual-origin", O_CREAT|O_EXCL|O_RDWR, 0600);
    if (nbd < 0 || file < 0 || ftruncate(file, SIZE_BYTES)) { perror("dual open"); return 2; }
    int sockets[2][2];
    for (int i = 0; i < 2; i++) if (socketpair(AF_UNIX, SOCK_STREAM, 0, sockets[i])) { perror("socketpair"); return 2; }
    for (int i = 0; i < 2; i++) {
        pid_t pid = fork();
        if (pid < 0) { perror("fork"); return 2; }
        if (!pid) {
            close(nbd);
            for (int j = 0; j < 2; j++) { close(sockets[j][0]); if (j != i) close(sockets[j][1]); }
            serve(sockets[i][1], file, i);
        }
        printf("DUAL_PEER_%d=%d\n", i, pid); fflush(stdout);
    }
    close(file);
    for (int i = 0; i < 2; i++) close(sockets[i][1]);
    if (ioctl(nbd, NBD_SET_BLKSIZE, 4096UL) || ioctl(nbd, NBD_SET_SIZE, SIZE_BYTES) ||
        ioctl(nbd, NBD_SET_FLAGS, NBD_FLAG_HAS_FLAGS|NBD_FLAG_SEND_FLUSH|NBD_FLAG_SEND_FUA|NBD_FLAG_CAN_MULTI_CONN) ||
        ioctl(nbd, NBD_SET_TIMEOUT, 30UL)) { perror("nbd configuration"); return 2; }
    for (int i = 0; i < 2; i++) {
        if (ioctl(nbd, NBD_SET_SOCK, sockets[i][0])) { perror("nbd socket"); return 2; }
        close(sockets[i][0]);
    }
    puts("DUAL_READY"); fflush(stdout);
    int rc = ioctl(nbd, NBD_DO_IT);
    printf("DUAL_DONE rc=%d errno=%d\n", rc, errno); fflush(stdout);
    ioctl(nbd, NBD_CLEAR_QUE);
    ioctl(nbd, NBD_CLEAR_SOCK);
    close(nbd);
    return 0;
}
C
gcc -O2 -static -o "$IRD/dualpeer" "$WORK/dualpeer.c"
;; esac

# Test-only kernel attach owner: negotiate two existing product listeners and
# retain the NBD ioctl until guest shutdown. The helper serves no data itself.
case " $SCENARIOS " in *" s19 "*|*" s20 "*)
cat > "$WORK/product_dual_attach.c" <<'C'
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <endian.h>
#include <errno.h>
#include <fcntl.h>
#include <linux/nbd.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

#include <linux/genetlink.h>
#include <linux/netlink.h>
#include <linux/nbd-netlink.h>
#include <stdlib.h>

static int add_attr(struct nlmsghdr *msg, size_t cap, uint16_t type, const void *data, size_t size) {
    size_t off = NLMSG_ALIGN(msg->nlmsg_len), len = NLA_HDRLEN + size;
    size_t aligned = NLA_ALIGN(len);
    if (off + aligned > cap || len > UINT16_MAX) return -1;
    struct nlattr *attr = (struct nlattr *)((char *)msg + off);
    attr->nla_type = type;
    attr->nla_len = (uint16_t)len;
    if (size) memcpy((char *)attr + NLA_HDRLEN, data, size);
    memset((char *)attr + len, 0, aligned - len);
    msg->nlmsg_len = (uint32_t)(off + aligned);
    return 0;
}
static int netlink_send(int fd, struct nlmsghdr *msg) {
    struct sockaddr_nl kernel = { .nl_family = AF_NETLINK };
    return sendto(fd, msg, msg->nlmsg_len, 0, (struct sockaddr *)&kernel, sizeof(kernel)) == (ssize_t)msg->nlmsg_len ? 0 : -1;
}
static int family_id(int fd) {
    char buf[4096] __attribute__((aligned(8))) = {0};
    struct nlmsghdr *msg = (struct nlmsghdr *)buf;
    msg->nlmsg_len = NLMSG_LENGTH(GENL_HDRLEN);
    msg->nlmsg_type = GENL_ID_CTRL; msg->nlmsg_flags = NLM_F_REQUEST; msg->nlmsg_seq = 1;
    struct genlmsghdr *genl = NLMSG_DATA(msg);
    genl->cmd = CTRL_CMD_GETFAMILY; genl->version = 1;
    const char name[] = NBD_GENL_FAMILY_NAME;
    if (add_attr(msg, sizeof(buf), CTRL_ATTR_FAMILY_NAME, name, sizeof(name)) || netlink_send(fd, msg)) return -1;
    ssize_t got = recv(fd, buf, sizeof(buf), 0);
    if (got < 0) return -1;
    int remaining = (int)got;
    for (msg = (struct nlmsghdr *)buf; NLMSG_OK(msg, remaining); msg = NLMSG_NEXT(msg, remaining)) {
        if (msg->nlmsg_seq != 1 || msg->nlmsg_type == NLMSG_ERROR || msg->nlmsg_type != GENL_ID_CTRL) continue;
        int attrs = (int)msg->nlmsg_len - (int)NLMSG_LENGTH(GENL_HDRLEN);
        struct nlattr *attr = (struct nlattr *)((char *)NLMSG_DATA(msg) + GENL_HDRLEN);
        while (attrs >= (int)sizeof(*attr) && attr->nla_len >= sizeof(*attr) && attr->nla_len <= attrs) {
            if ((attr->nla_type & NLA_TYPE_MASK) == CTRL_ATTR_FAMILY_ID && attr->nla_len == NLA_HDRLEN + sizeof(uint16_t)) {
                uint16_t id; memcpy(&id, (char *)attr + NLA_HDRLEN, sizeof(id)); return id;
            }
            int step = NLA_ALIGN(attr->nla_len); attrs -= step; attr = (struct nlattr *)((char *)attr + step);
        }
    }
    return -1;
}
static int open_netlink(void) {
    int fd = socket(AF_NETLINK, SOCK_RAW, NETLINK_GENERIC);
    if (fd < 0) return -1;
    struct timeval timeout = { .tv_sec = 10, .tv_usec = 0 };
    struct sockaddr_nl local = { .nl_family = AF_NETLINK };
    if (setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout)) ||
        bind(fd, (struct sockaddr *)&local, sizeof(local))) { close(fd); return -1; }
    return fd;
}
static const char backend_id[] = "ramshared-qemu-origin-vdb1";
static int reconfigure(int socket_fd, const char *identifier) {
    int fd = open_netlink();
    if (fd < 0) return -1;
    int family = family_id(fd);
    if (family < 0) { close(fd); return -1; }
    char buf[1024] __attribute__((aligned(8))) = {0};
    struct nlmsghdr *msg = (struct nlmsghdr *)buf;
    msg->nlmsg_len = NLMSG_LENGTH(GENL_HDRLEN);
    msg->nlmsg_type = (uint16_t)family; msg->nlmsg_flags = NLM_F_REQUEST | NLM_F_ACK; msg->nlmsg_seq = 2;
    struct genlmsghdr *genl = NLMSG_DATA(msg);
    genl->cmd = NBD_CMD_RECONFIGURE; genl->version = NBD_GENL_VERSION;
    uint32_t index = 0, socket_value = (uint32_t)socket_fd;
    if (add_attr(msg, sizeof(buf), NBD_ATTR_INDEX, &index, sizeof(index)) ||
        add_attr(msg, sizeof(buf), NBD_ATTR_BACKEND_IDENTIFIER, identifier, strlen(identifier) + 1)) { close(fd); return -1; }
    size_t outer = NLMSG_ALIGN(msg->nlmsg_len);
    if (add_attr(msg, sizeof(buf), NBD_ATTR_SOCKETS | NLA_F_NESTED, NULL, 0)) { close(fd); return -1; }
    size_t inner = NLMSG_ALIGN(msg->nlmsg_len);
    if (add_attr(msg, sizeof(buf), NBD_SOCK_ITEM | NLA_F_NESTED, NULL, 0) ||
        add_attr(msg, sizeof(buf), NBD_SOCK_FD, &socket_value, sizeof(socket_value))) { close(fd); return -1; }
    ((struct nlattr *)(buf + inner))->nla_len = (uint16_t)(msg->nlmsg_len - inner);
    ((struct nlattr *)(buf + outer))->nla_len = (uint16_t)(msg->nlmsg_len - outer);
    if (netlink_send(fd, msg)) { close(fd); return -1; }
    ssize_t got = recv(fd, buf, sizeof(buf), 0);
    close(fd);
    if (got < (ssize_t)NLMSG_LENGTH(sizeof(struct nlmsgerr))) return -1;
    msg = (struct nlmsghdr *)buf;
    if (msg->nlmsg_seq != 2 || msg->nlmsg_type != NLMSG_ERROR) return -1;
    struct nlmsgerr *error = NLMSG_DATA(msg);
    if (error->error) { errno = -error->error; return -1; }
    return 0;
}

static int initial_netlink_connect(int first, int second, uint64_t size, uint16_t flags) {
    int fd = open_netlink();
    if (fd < 0) return -1;
    int family = family_id(fd);
    if (family < 0) { close(fd); return -1; }
    char buf[1024] __attribute__((aligned(8))) = {0};
    struct nlmsghdr *msg = (struct nlmsghdr *)buf;
    msg->nlmsg_len = NLMSG_LENGTH(GENL_HDRLEN);
    msg->nlmsg_type = (uint16_t)family; msg->nlmsg_flags = NLM_F_REQUEST | NLM_F_ACK; msg->nlmsg_seq = 3;
    struct genlmsghdr *genl = NLMSG_DATA(msg);
    genl->cmd = NBD_CMD_CONNECT; genl->version = NBD_GENL_VERSION;
    uint32_t index = 0;
    uint64_t block_size = 4096, timeout = 30, server_flags = flags;
    if (add_attr(msg, sizeof(buf), NBD_ATTR_INDEX, &index, sizeof(index)) ||
        add_attr(msg, sizeof(buf), NBD_ATTR_SIZE_BYTES, &size, sizeof(size)) ||
        add_attr(msg, sizeof(buf), NBD_ATTR_BLOCK_SIZE_BYTES, &block_size, sizeof(block_size)) ||
        add_attr(msg, sizeof(buf), NBD_ATTR_TIMEOUT, &timeout, sizeof(timeout)) ||
        add_attr(msg, sizeof(buf), NBD_ATTR_SERVER_FLAGS, &server_flags, sizeof(server_flags)) ||
        add_attr(msg, sizeof(buf), NBD_ATTR_BACKEND_IDENTIFIER, backend_id, sizeof(backend_id))) { close(fd); return -1; }
    size_t outer = NLMSG_ALIGN(msg->nlmsg_len);
    if (add_attr(msg, sizeof(buf), NBD_ATTR_SOCKETS | NLA_F_NESTED, NULL, 0)) { close(fd); return -1; }
    int peers[] = { first, second };
    for (size_t i = 0; i < 2; i++) {
        size_t inner = NLMSG_ALIGN(msg->nlmsg_len);
        uint32_t socket_value = (uint32_t)peers[i];
        if (add_attr(msg, sizeof(buf), NBD_SOCK_ITEM | NLA_F_NESTED, NULL, 0) ||
            add_attr(msg, sizeof(buf), NBD_SOCK_FD, &socket_value, sizeof(socket_value))) { close(fd); return -1; }
        ((struct nlattr *)(buf + inner))->nla_len = (uint16_t)(msg->nlmsg_len - inner);
    }
    ((struct nlattr *)(buf + outer))->nla_len = (uint16_t)(msg->nlmsg_len - outer);
    if (netlink_send(fd, msg)) { close(fd); return -1; }
    int confirmed_index = 0, acknowledged = 0;
    for (int tries = 0; tries < 3 && !acknowledged; tries++) {
        ssize_t got = recv(fd, buf, sizeof(buf), 0);
        if (got < 0) { close(fd); return -1; }
        int remaining = (int)got;
        for (msg = (struct nlmsghdr *)buf; NLMSG_OK(msg, remaining); msg = NLMSG_NEXT(msg, remaining)) {
            if (msg->nlmsg_seq != 3) continue;
            if (msg->nlmsg_type == NLMSG_ERROR) {
                if (msg->nlmsg_len < NLMSG_LENGTH(sizeof(struct nlmsgerr))) { close(fd); return -1; }
                struct nlmsgerr *error = NLMSG_DATA(msg);
                if (error->error) { errno = -error->error; close(fd); return -1; }
                acknowledged = 1;
            } else if (msg->nlmsg_type == (uint16_t)family && msg->nlmsg_len >= NLMSG_LENGTH(GENL_HDRLEN)) {
                int attrs = (int)msg->nlmsg_len - (int)NLMSG_LENGTH(GENL_HDRLEN);
                struct nlattr *attr = (struct nlattr *)((char *)NLMSG_DATA(msg) + GENL_HDRLEN);
                while (attrs >= (int)sizeof(*attr) && attr->nla_len >= sizeof(*attr) && attr->nla_len <= attrs) {
                    if ((attr->nla_type & NLA_TYPE_MASK) == NBD_ATTR_INDEX && attr->nla_len == NLA_HDRLEN + sizeof(uint32_t)) {
                        uint32_t got_index; memcpy(&got_index, (char *)attr + NLA_HDRLEN, sizeof(got_index));
                        if (got_index == index) confirmed_index = 1;
                    }
                    int step = NLA_ALIGN(attr->nla_len); attrs -= step; attr = (struct nlattr *)((char *)attr + step);
                }
            }
        }
    }
    close(fd);
    return confirmed_index && acknowledged ? 0 : -1;
}

static int transfer(int fd, void *buf, size_t len, int writing) {
    size_t off = 0;
    while (off < len) {
        ssize_t n = writing ? write(fd, (char *)buf + off, len - off)
                            : read(fd, (char *)buf + off, len - off);
        if (n < 0 && errno == EINTR) continue;
        if (n <= 0) return -1;
        off += (size_t)n;
    }
    return 0;
}
static int connect_peer(const char *path, uint64_t *size, uint16_t *flags) {
    int fd = socket(AF_UNIX, SOCK_STREAM, 0);
    if (fd < 0) return -1;
    struct timeval timeout = { .tv_sec = 10 }, no_timeout = {0};
    if (setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout)) ||
        setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &timeout, sizeof(timeout))) { close(fd); return -1; }
    struct sockaddr_un addr = { .sun_family = AF_UNIX };
    if (strlen(path) >= sizeof(addr.sun_path)) return -1;
    strcpy(addr.sun_path, path);
    if (connect(fd, (struct sockaddr *)&addr, sizeof(addr))) return -1;
    uint64_t magic, opts;
    uint16_t handshake_flags;
    if (transfer(fd, &magic, 8, 0) || transfer(fd, &opts, 8, 0) ||
        transfer(fd, &handshake_flags, 2, 0) ||
        be64toh(magic) != 0x4e42444d41474943ULL ||
        be64toh(opts) != 0x49484156454f5054ULL ||
        (ntohs(handshake_flags) & 3) != 3) return -1;
    uint32_t client_flags = htonl(3), option = htonl(1), name_len = htonl(7);
    opts = htobe64(0x49484156454f5054ULL);
    char name[] = "default";
    if (transfer(fd, &client_flags, 4, 1) || transfer(fd, &opts, 8, 1) ||
        transfer(fd, &option, 4, 1) || transfer(fd, &name_len, 4, 1) ||
        transfer(fd, name, 7, 1) || transfer(fd, size, 8, 0) ||
        transfer(fd, flags, 2, 0)) return -1;
    *size = be64toh(*size);
    *flags = ntohs(*flags);
    if (setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &no_timeout, sizeof(no_timeout)) ||
        setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &no_timeout, sizeof(no_timeout))) { close(fd); return -1; }
    return fd;
}
int main(int argc, char **argv) {
    if (argc == 2 && (!strcmp(argv[1], "--replace") || !strcmp(argv[1], "--replace-wrong"))) {
        uint64_t size; uint16_t flags;
        int peer = connect_peer("/run/product-peer2.sock", &size, &flags);
        if (peer < 0 || size != 1073741824ULL || !(flags & NBD_FLAG_CAN_MULTI_CONN)) {
            perror("product peer replacement"); return 2;
        }
        if (!strcmp(argv[1], "--replace-wrong")) {
            int rc = reconfigure(peer, "wrong-origin");
            int error = errno;
            close(peer);
            if (rc != -1 || error != EINVAL) return 2;
            puts("PRODUCT_WRONG_BACKEND_REFUSED");
            return 0;
        }
        if (reconfigure(peer, backend_id)) { perror("product peer replacement"); return 2; }
        close(peer);
        puts("PRODUCT_REATTACH_OK");
        return 0;
    }
    int use_netlink = argc == 2 && !strcmp(argv[1], "--netlink");
    if (argc != 1 && !use_netlink) return 2;
    uint64_t size0, size1;
    uint16_t flags0, flags1;
    int a = connect_peer("/run/product-peer0.sock", &size0, &flags0);
    int b = connect_peer("/run/product-peer1.sock", &size1, &flags1);
    if (a < 0 || b < 0 || size0 != size1 || size0 != 1073741824ULL ||
        flags0 != flags1 || !(flags0 & NBD_FLAG_CAN_MULTI_CONN)) {
        fprintf(stderr, "product peer handshake refused\n"); return 2;
    }
    if (use_netlink) {
        if (initial_netlink_connect(a, b, size0, flags0)) { perror("product netlink attach"); return 2; }
        close(a); close(b);
        puts("PRODUCT_DUAL_READY"); fflush(stdout);
        for (;;) pause();
    }
    int nbd = open("/dev/nbd0", O_RDWR);
    if (nbd < 0 || ioctl(nbd, NBD_SET_BLKSIZE, 4096UL) ||
        ioctl(nbd, NBD_SET_SIZE, (unsigned long)size0) ||
        ioctl(nbd, NBD_SET_FLAGS, (unsigned long)flags0) ||
        ioctl(nbd, NBD_SET_TIMEOUT, 30UL) || ioctl(nbd, NBD_SET_SOCK, a) ||
        ioctl(nbd, NBD_SET_SOCK, b)) {
        perror("product dual attach"); return 2;
    }
    close(a); close(b);
    puts("PRODUCT_DUAL_READY"); fflush(stdout);
    int rc = ioctl(nbd, NBD_DO_IT);
    fprintf(stderr, "product dual ended rc=%d errno=%d\n", rc, errno);
    ioctl(nbd, NBD_CLEAR_QUE); ioctl(nbd, NBD_CLEAR_SOCK); close(nbd);
    return rc != 0;
}
C
gcc -O2 -Wall -Wextra -Werror -static -o "$IRD/product-dual-attach" "$WORK/product_dual_attach.c"
;; esac

# O_DIRECT read of one block: a buffered read can be served from cache for a
# short window after the peer dies, which would read a
# corpse as alive.
cat > "$WORK/directread.c" <<'C'
#define _GNU_SOURCE
#include <fcntl.h>
#include <stdio.h>
#include <unistd.h>
#include <stdlib.h>
#include <errno.h>
#include <string.h>
int main(int argc, char **argv) {
    if (argc < 2) return 2;
    int fd = open(argv[1], O_RDONLY | O_DIRECT);
    if (fd < 0) { fprintf(stderr, "open: %s\n", strerror(errno)); return 2; }
    void *buf;
    if (posix_memalign(&buf, 4096, 4096) != 0) return 2;
    ssize_t n = pread(fd, buf, 4096, 0);
    if (n < 0) { fprintf(stderr, "read: %s\n", strerror(errno)); return 1; }
    printf("read %zd\n", n);
    return 0;
}
C
gcc -O1 -static -o "$IRD/directread" "$WORK/directread.c"

# Functional integrity helper: preserve the swap header and verify every byte
# through both the exported device and its separate authoritative origin.
cat > "$WORK/origincheck.c" <<'C'
#define _GNU_SOURCE
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

int main(int argc, char **argv)
{
	unsigned char *expected, *observed;
	int write_mode, fd, origin = -1;

	if (argc < 3 || (strcmp(argv[1], "write") && strcmp(argv[1], "read")))
		return 2;
	write_mode = !strcmp(argv[1], "write");
	if (write_mode && argc != 4)
		return 2;
	if (posix_memalign((void **)&expected, 4096, 4096) ||
	    posix_memalign((void **)&observed, 4096, 4096))
		return 2;
	fd = open(argv[2], (write_mode ? O_RDWR : O_RDONLY) | O_DIRECT);
	if (fd < 0) { perror("open exported/origin device"); return 1; }
	if (write_mode) {
		origin = open(argv[3], O_RDONLY | O_DIRECT);
		if (origin < 0) { perror("open raw origin"); return 1; }
		for (int page = 0; page < 16; page++) {
			for (int i = 0; i < 4096; i++)
				expected[i] = (unsigned char)((i + page * 17) % 251);
			if (pwrite(fd, expected, 4096, 1048576 + page * 4096) != 4096)
				{ perror("write payload"); return 1; }
		}
		if (fsync(fd)) { perror("flush payload"); return 1; }
	}
	for (int page = 0; page < 16; page++) {
		for (int i = 0; i < 4096; i++)
			expected[i] = (unsigned char)((i + page * 17) % 251);
		if (pread(fd, observed, 4096, 1048576 + page * 4096) != 4096 ||
		    memcmp(expected, observed, 4096))
			{ fprintf(stderr, "device mismatch page=%d\n", page); return 1; }
		if (origin >= 0 &&
		    (pread(origin, observed, 4096, 1048576 + page * 4096) != 4096 ||
		     memcmp(expected, observed, 4096)))
			{ fprintf(stderr, "origin mismatch page=%d\n", page); return 1; }
	}
	if (origin >= 0 && close(origin))
		return 1;
	if (close(fd))
		return 1;
	free(expected);
	free(observed);
	puts("verified_bytes=65536 mismatches=0");
	return 0;
}
C
gcc -O2 -Wall -Wextra -Werror -static -o "$IRD/origincheck" "$WORK/origincheck.c"

cat > "$WORK/pagetest.c" <<'C'
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <errno.h>
#include <sys/mman.h>
int main() {
    size_t mb = 32, chunk = 1024*1024;
    char **c = calloc(mb, sizeof(char*));
    for (size_t i=0;i<mb;i++){ if(posix_memalign((void**)&c[i],4096,chunk)) return 2; memset(c[i],i,chunk); }
    int fails=0;
    for (size_t i=0;i<mb;i++) if (madvise(c[i], chunk, MADV_PAGEOUT)!=0) fails++;
    FILE *f=fopen("/proc/self/status","r"); char line[256];
    while(fgets(line,sizeof line,f)) if(!strncmp(line,"VmRSS",5)) printf("pagetest after: %s",line);
    fclose(f);
    printf("pagetest fails=%d\n", fails);
    return 0;
}
C
gcc -O1 -static -o "$IRD/pagetest" "$WORK/pagetest.c"

# The spill leg's pressure source: the same page-out shape as memhog but
# with pseudo-random content, so the measured compression ratio is the
# kernel's real one (a constant-per-chunk fill compresses to nothing and
# would report a synthetic ratio).
cat > "$WORK/randhog.c" <<'C'
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <signal.h>
#include <time.h>
#include <sys/mman.h>

static volatile sig_atomic_t do_touch_hot = 0;
static volatile sig_atomic_t do_touch_spilled = 0;
static void on_usr1(int sig) { (void)sig; do_touch_hot = 1; }
static void on_usr2(int sig) { (void)sig; do_touch_spilled = 1; }
static double now_s(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec + ts.tv_nsec / 1e9;
}

int main(int argc, char **argv) {
    size_t mb = argc > 1 ? (size_t)atoi(argv[1]) : 192;
    size_t out_mb = argc > 2 ? (size_t)atoi(argv[2]) : 96;
    size_t chunk = 1024 * 1024;
    char **chunks = calloc(mb, sizeof(char *));
    if (!chunks) return 2;
    unsigned long long s = 0x9E3779B97F4A7C15ULL;
    for (size_t i = 0; i < mb; i++) {
        if (posix_memalign((void **)&chunks[i], 4096, chunk)) return 2;
        for (size_t o = 0; o < chunk; o += 8) {
            s ^= s << 13; s ^= s >> 7; s ^= s << 17;
            memcpy(chunks[i] + o, &s, 8);
        }
    }
    for (size_t i = 0; i < out_mb && i < mb; i++) madvise(chunks[i], chunk, MADV_PAGEOUT);
    printf("allocated=%zu paged_out=%zu holding\n", mb, out_mb);
    fflush(stdout);
    signal(SIGUSR1, on_usr1);
    signal(SIGUSR2, on_usr2);
    /* SIGUSR1 touches the HOT region (the chunks that were not paged out):
     * they live in the zram's RAM, so a dead tier must not touch them.
     * SIGUSR2 touches the SPILLED region (the chunks that were paged out and
     * may have been written back to the tier): with the tier dead those
     * faults are the exposure the drill measures. */
    for (;;) {
        pause();
        unsigned long sum = 0;
        double t0 = now_s();
        if (do_touch_hot) {
            do_touch_hot = 0;
            for (size_t i = out_mb; i < mb; i++)
                for (size_t j = 0; j < chunk; j += 4096) sum += (unsigned char)chunks[i][j];
            printf("hot_touch_ms=%.0f sum=%lu\n", (now_s() - t0) * 1000, sum);
        } else {
            do_touch_spilled = 0;
            for (size_t i = 0; i < out_mb && i < mb; i++)
                for (size_t j = 0; j < chunk; j += 4096) sum += (unsigned char)chunks[i][j];
            printf("spilled_touch_ms=%.0f sum=%lu\n", (now_s() - t0) * 1000, sum);
        }
        fflush(stdout);
    }
}
C
gcc -O2 -static -o "$IRD/randhog" "$WORK/randhog.c"

# swapon with an explicit priority: busybox's swapon has no -p, and the
# zram-first order is the topology under test.
cat > "$WORK/swapon_prio.c" <<'C'
#define _GNU_SOURCE
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/mount.h>
#include <sys/swap.h>
int main(int argc, char **argv) {
    if (argc != 3) return 2;
    char *end = NULL;
    errno = 0;
    long prio = strtol(argv[2], &end, 10);
    if (errno || end == argv[2] || *end || prio < 0 || prio > SWAP_FLAG_PRIO_MASK) return 2;
    int flags = SWAP_FLAG_PREFER | (((int)prio << SWAP_FLAG_PRIO_SHIFT) & SWAP_FLAG_PRIO_MASK);
    if (swapon(argv[1], flags) != 0) { perror("swapon"); return 1; }
    return 0;
}
C
gcc -O2 -static -o "$IRD/swapon_prio" "$WORK/swapon_prio.c"

# 4 KiB random I/O rate and latency, the s6 leg's measuring instrument.
# Per-thread xorshift offsets, O_DIRECT, one histogram; reports ops,
# seconds, mbs, iops, p50 and p95 in microseconds.
cat > "$WORK/rate.c" <<'C'
#define _GNU_SOURCE
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>
#include <pthread.h>

static long ops_per_thread;
static int fd;
static off_t span;
static int write_mode;
static double *lat;
static int threads;

static double now_s(void){ struct timespec ts; clock_gettime(CLOCK_MONOTONIC,&ts); return ts.tv_sec + ts.tv_nsec/1e9; }
struct arg { int idx; unsigned long seed; };

static void *worker(void *p){
    struct arg *a = p;
    unsigned long s = a->seed ? a->seed : 88172645463325252UL;
    char *buf; if (posix_memalign((void**)&buf, 4096, 4096)) return NULL;
    memset(buf, (int)a->idx, 4096);
    for (long i=0;i<ops_per_thread;i++){
        s ^= s<<13; s ^= s>>7; s ^= s<<17;
        off_t off = (off_t)((s % (unsigned long)(span/4096))) * 4096;
        double t0 = now_s();
        ssize_t n = write_mode ? pwrite(fd, buf, 4096, off) : pread(fd, buf, 4096, off);
        double t1 = now_s();
        if (n != 4096) fprintf(stderr, "io err off=%lld n=%zd\n", (long long)off, n);
        lat[a->idx*ops_per_thread + i] = (t1-t0)*1e6;
    }
    free(buf); return NULL;
}
static int cmp(const void *a, const void *b){ double x=*(double*)a,y=*(double*)b; return x<y?-1:(x>y?1:0); }
int main(int argc,char**argv){
    if (argc<6) return 2;
    const char *dev=argv[1]; threads=atoi(argv[2]); ops_per_thread=atol(argv[3]); span=(off_t)atoll(argv[4])*1024*1024; write_mode = argv[5][0]=='w';
    fd = open(dev, O_RDWR|O_DIRECT);
    if (fd<0){ perror("open"); return 2; }
    lat = malloc(sizeof(double)*threads*ops_per_thread);
    pthread_t th[64]; struct arg a[64];
    double t0=now_s();
    for (int i=0;i<threads;i++){ a[i].idx=i; a[i].seed=0x9e3779b97f4a7c15UL*(unsigned long)(i+1); pthread_create(&th[i],NULL,worker,&a[i]); }
    for (int i=0;i<threads;i++) pthread_join(th[i],NULL);
    double t1=now_s();
    long n=threads*ops_per_thread;
    qsort(lat, n, sizeof(double), cmp);
    double secs=t1-t0;
    printf("ops=%ld seconds=%.3f mbs=%.1f iops=%.0f p50_us=%.0f p95_us=%.0f\n", n, secs, (double)n*4096.0/1048576.0/secs, (double)n/secs, lat[n/2], lat[(long)(n*0.95)]);
    return 0;
}
C
gcc -O2 -static -pthread -o "$IRD/rate" "$WORK/rate.c"

# The swap-path leg's pressure source (scenario s7): the hog pages itself
# out, and on SIGUSR1 times the touch that faults every page back in.
cat > "$WORK/swaphog.c" <<'C'
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <signal.h>
#include <time.h>
#include <unistd.h>
#include <sys/mman.h>

static char **chunks;
static size_t g_mb;
static size_t chunk = 1024 * 1024;
static volatile sig_atomic_t do_touch = 0, do_pageout = 0;
static void on_usr1(int sig) { (void)sig; do_touch = 1; }
static void on_usr2(int sig) { (void)sig; do_pageout = 1; }
static double now_s(void) { struct timespec ts; clock_gettime(CLOCK_MONOTONIC, &ts); return ts.tv_sec + ts.tv_nsec / 1e9; }

int main(int argc, char **argv) {
    g_mb = argc > 1 ? (size_t)atoi(argv[1]) : 128;
    chunks = calloc(g_mb, sizeof(char *));
    if (!chunks) return 2;
    for (size_t i = 0; i < g_mb; i++) {
        if (posix_memalign((void **)&chunks[i], 4096, chunk)) return 2;
        memset(chunks[i], (int)(i & 0xff), chunk);
    }
    for (size_t i = 0; i < g_mb; i++) madvise(chunks[i], chunk, MADV_PAGEOUT);
    printf("allocated=%zu paged_out=%zu\n", g_mb, g_mb);
    fflush(stdout);
    signal(SIGUSR1, on_usr1);
    signal(SIGUSR2, on_usr2);
    for (;;) {
        pause();
        if (do_pageout) {
            do_pageout = 0;
            double t0 = now_s();
            for (size_t i = 0; i < g_mb; i++) madvise(chunks[i], chunk, MADV_PAGEOUT);
            printf("pageout_ms=%.0f\n", (now_s() - t0) * 1000);
            fflush(stdout);
        }
        if (do_touch) {
            do_touch = 0;
            unsigned long sum = 0;
            double t0 = now_s();
            for (size_t i = 0; i < g_mb; i++)
                for (size_t j = 0; j < chunk; j += 4096) sum += (unsigned char)chunks[i][j];
            double dt = now_s() - t0;
            printf("touch_ms=%.0f mb=%zu mbs=%.1f sum=%lu\n", dt * 1000, g_mb, (double)g_mb / dt, sum);
            fflush(stdout);
        }
    }
}
C
gcc -O2 -static -o "$IRD/swaphog" "$WORK/swaphog.c"



# A swap signature at a byte offset: the s6 origin partition must present
# PARTUUID/UUID/TYPE=swap to blkid without needing a loop device (the
# campaign host may not have one free, and losetup needs root).
cat > "$WORK/swapsig.c" <<'C'
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
int main(int argc, char **argv) {
    if (argc < 4) return 2;
    const char *path = argv[1];
    long off = atol(argv[2]);
    const char *uuid = argv[3];
    unsigned char page[4096];
    memset(page, 0, sizeof page);
    unsigned int version = 1;
    memcpy(page + 1024, &version, 4);
    /* libblkid's swap probe requires a non-zero last_page (the swap header
     * is not recognized from the magic alone). The file size is the honest
     * upper bound the helper can see; the value is informational. */
    FILE *f0 = fopen(path, "rb");
    if (!f0) { perror("open"); return 2; }
    fseek(f0, 0, SEEK_END);
    long size = ftell(f0);
    fclose(f0);
    /* The swap area the signature describes: the daemon's logical device
     * is smaller than the file, and the kernel refuses a swap area shorter
     * than the signature indicates. argv[4] (MiB) overrides the file size. */
    unsigned int last_page = 0;
    if (argc > 4) {
        last_page = (unsigned int)((unsigned long long)atoll(argv[4]) * 1024 * 1024 / 4096 - 1);
    } else if (size > off + 4096) {
        last_page = (unsigned int)((size - off) / 4096 - 1);
    }
    memcpy(page + 1028, &last_page, 4);
    /* uuid bytes: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee -> RFC4122 order */
    unsigned int a; unsigned int b0, b1, b2; unsigned long long c;
    if (sscanf(uuid, "%8x-%4x-%4x-%4x-%12llx", &a, &b0, &b1, &b2, &c) != 5) return 2;
    page[1036] = (a >> 24) & 0xff; page[1037] = (a >> 16) & 0xff; page[1038] = (a >> 8) & 0xff; page[1039] = a & 0xff;
    page[1040] = (b0 >> 8) & 0xff; page[1041] = b0 & 0xff;
    page[1042] = (b1 >> 8) & 0xff; page[1043] = b1 & 0xff;
    page[1044] = (b2 >> 8) & 0xff; page[1045] = b2 & 0xff;
    page[1046] = (c >> 40) & 0xff; page[1047] = (c >> 32) & 0xff; page[1048] = (c >> 24) & 0xff;
    page[1049] = (c >> 16) & 0xff; page[1050] = (c >> 8) & 0xff; page[1051] = c & 0xff;
    memcpy(page + 4086, "SWAPSPACE2", 10);
    FILE *f = fopen(path, "r+b");
    if (!f) { perror("open"); return 2; }
    if (fseek(f, off, SEEK_SET) != 0) return 2;
    if (fwrite(page, 1, sizeof page, f) != sizeof page) return 2;
    fclose(f);
    return 0;
}
C
gcc -O2 -static -o "$WORK/swapsig" "$WORK/swapsig.c"

# s6's guest-side scenario: the sealed-origin chain built from the guest's
# own device identity, the daemon in origin mode, and the rate leg. The
# manifests are generated here because the identity (PARTUUID, dev_t) is
# only known at boot; the hash chain is the production one.
cat > "$IRD/origin_env.sh" <<'ENV'
#!/bin/busybox sh
BB=/bin/busybox
log() { echo "KTEST $*"; }
iops_of() { echo "$1" | $BB sed 's/.*iops=\([0-9]*\).*/\1/'; }
p50_of() { echo "$1" | $BB sed 's/.*p50_us=\([0-9]*\).*/\1/'; }
if [ -d /sys/module/nbd ]; then
  log "nbd_module=ALREADY_LOADED"
else
  $BB insmod /modules/nbd.ko nbds_max=16 2>/dev/null && log "nbd_module=OK" || log "nbd_module=FAIL"
fi
log "daemon_sha=$($BB sha256sum /ramsharedd | $BB cut -d' ' -f1)"
PUUID=$(blkid -s PARTUUID -o value /dev/vdb1 | $BB tr -d '\r\n ')
SUUID=$(blkid -s UUID -o value /dev/vdb1 | $BB tr -d '\r\n ')
PTUUID=$(blkid -s PTUUID -o value /dev/vdb | $BB tr -d '\r\n ')
PDT=$($BB cat /sys/class/block/vdb1/dev)
PDP=$($BB cat /sys/class/block/vdb/dev)
log "identity partuuid=$PUUID swap_uuid=$SUUID ptuuid=$PTUUID dev_t=$PDT parent=$PDP"
$BB mkdir -p /mnt/c/ProgramData/RamShared /etc/ramshared /dev/disk/by-partuuid /run /tmp
CONF=/tmp/hostcfg.txt
printf 'schema=3\norigin_vhdx=%s\nfixed_size_bytes=%s\nlogical_capacity_mib=%s\nphysical_cache_cap_mib=%s\nchunk_mib=%s\ngpu_reserve_min_mib=%s\ngpu_reserve_percent=%s\npartuuid=%s\ndisk_guid=%s\nexpected_swap_uuid=%s\nownership_proof_schema=%s\nexisting_wsl_swap_vhdx=%s\n' \
  'C:\ramshared\origin.vhdx' 6442450944 1024 1024 128 2048 20 "$PUUID" "$PTUUID" "$SUUID" 1 'D:\wsl-swap.vhdx' > $CONF
CFGSHA=$($BB sha256sum $CONF | $BB cut -d' ' -f1)
printf '{"schema_version":3,"origin_vhdx":"C:\\\\ramshared\\\\origin.vhdx","fixed_size_bytes":6442450944,"logical_capacity_mib":1024,"physical_cache_cap_mib":1024,"chunk_mib":128,"gpu_reserve_min_mib":2048,"gpu_reserve_percent":20,"partuuid":"%s","disk_guid":"%s","expected_swap_uuid":"%s","ownership_proof_schema":1,"existing_wsl_swap_vhdx":"D:\\\\wsl-swap.vhdx","configuration_sha256":"%s"}' "$PUUID" "$PTUUID" "$SUUID" "$CFGSHA" > /mnt/c/ProgramData/RamShared/ramshared-origin-manifest.json
HOSTSHA=$($BB sha256sum /mnt/c/ProgramData/RamShared/ramshared-origin-manifest.json | $BB cut -d' ' -f1)
ln -sf ../../vdb1 /dev/disk/by-partuuid/$PUUID
printf 'schema_version=3\nhost_manifest_sha256=%s\nconfiguration_sha256=%s\norigin_path=/dev/disk/by-partuuid/%s\npartuuid=%s\nptuuid=%s\npartition_dev_t=%s\nparent_dev_t=%s\nexpected_swap_uuid=%s\nswap_type=swap\nlogical_capacity_mib=1024\nphysical_cache_cap_mib=1024\n' \
  "$HOSTSHA" "$CFGSHA" "$PUUID" "$PUUID" "$PTUUID" "$PDT" "$PDP" "$SUUID" > /etc/ramshared/origin.conf
log "manifests_written"
start_daemon() {
  local threads=$1
  $BB rm -f /run/nbd.sock
  /ramsharedd --origin-manifest /etc/ramshared/origin.conf --accept-wsl2-nbd --size 1024 \
    --sock /run/nbd.sock --nbd /dev/nbd0 --serve-threads $threads >/tmp/daemon.log 2>&1 &
  DAEMON_PID=$!
  local i=0
  while [ $i -lt 15 ]; do
    $BB sleep 1
    if [ -S /run/nbd.sock ]; then log "daemon_up threads=$threads"; return 0; fi
    i=$((i+1))
  done
  log "daemon_start=FAIL"
  $BB tail -6 /tmp/daemon.log
  return 1
}
stop_daemon() {
  /nbd-client -d /dev/nbd0 >/dev/null 2>&1
  $BB kill $(pidof ramsharedd) 2>/dev/null
  $BB sleep 2
}
attach() {
  /nbd-client -N default -unix /run/nbd.sock /dev/nbd0 >/tmp/nbd.log 2>&1
  log "attach_rc=$? size=$($BB blockdev --getsize64 /dev/nbd0 2>/dev/null)"
}
start_daemon_with_pump() {
  local threads=$1 tier=$2
  $BB rm -f /run/nbd.sock
  /ramsharedd --origin-manifest /etc/ramshared/origin.conf --accept-wsl2-nbd --size 1024 \
    --sock /run/nbd.sock --nbd /dev/nbd0 --serve-threads $threads --zram-pump-tier $tier \
    >/tmp/daemon.log 2>&1 &
  local i=0
  while [ $i -lt 15 ]; do
    $BB sleep 1
    if [ -S /run/nbd.sock ]; then log "daemon_up threads=$threads pump=$tier"; return 0; fi
    i=$((i+1))
  done
  log "daemon_start=FAIL"
  $BB tail -6 /tmp/daemon.log
  return 1
}

ENV

cat > "$IRD/s12.sh" <<'S12'
#!/bin/busybox sh
. /origin_env.sh
fail() {
  log "scenario_failed=s12 reason=$1"
  $BB tail -n 30 /tmp/daemon.log 2>/dev/null
  $BB dmesg | $BB tail -n 20
  $BB poweroff -f
  exit 1
}
rows() { $BB awk -v target="$1" 'NR>1 && $1==target {n++} END {print n+0}' /proc/swaps; }
header_hash() {
  local device=${1:-/dev/vdb1} snapshot=${2:-/tmp/origin-header} digest
  $BB dd if="$device" of="$snapshot" bs=4096 count=16 2>/dev/null || return 1
  [ "$($BB stat -c %s "$snapshot")" = 65536 ] || return 1
  digest=$($BB sha256sum "$snapshot") || return 1
  printf '%s\n' "${digest%% *}"
}
log "scenario=s12"
EXPECTED=$($BB cat /expected-ramsharedd.sha256)
ACTUAL=$($BB sha256sum /ramsharedd | $BB cut -d' ' -f1)
[ "${#EXPECTED}" = 64 ] && [ "$EXPECTED" = "$ACTUAL" ] || fail binary_mismatch
log "gate_daemon_binary_match=PASS sha256=$ACTUAL"
$BB swapon /dev/vdc1 || fail fallback_activation
[ "$(rows /dev/vdc1)" = 1 ] || fail fallback_not_observed
HEADER=$(header_hash) || fail initial_header_read
$BB cp /etc/ramshared/origin.conf /tmp/origin.conf.saved
expect_startup_refusal() {
  local label=$1 expected=$2 pid i=0 rc
  $BB rm -f /run/nbd.sock
  /ramsharedd --origin-manifest /etc/ramshared/origin.conf --accept-wsl2-nbd --size 1024 \
    --sock /run/nbd.sock --nbd /dev/nbd0 >/tmp/daemon.log 2>&1 & pid=$!
  while $BB kill -0 "$pid" 2>/dev/null; do
    [ "$i" -lt 20 ] || fail "${label}_did_not_exit"
    $BB sleep 1
    i=$((i+1))
  done
  wait "$pid"; rc=$?
  [ "$rc" != 0 ] || fail "${label}_false_success"
  $BB grep -Fq "$expected" /tmp/daemon.log || fail "${label}_wrong_refusal"
  [ ! -S /run/nbd.sock ] || fail "${label}_served_export"
  [ "$(rows /dev/nbd0)" = 0 ] || fail "${label}_armed_tier"
  [ "$(rows /dev/vdc1)" = 1 ] || fail "${label}_lost_fallback"
  [ "$(header_hash)" = "$HEADER" ] || fail "${label}_changed_header"
  log "startup_refusal=$label exit=$rc"
}
$BB ln /etc/ramshared/origin.conf /tmp/origin.conf.alias || fail seal_link_fixture
expect_startup_refusal hardlinked_seal "bounded, singly linked sealed regular file"
$BB rm /tmp/origin.conf.alias || fail seal_link_cleanup
$BB rm /etc/ramshared/origin.conf || fail seal_fifo_replace
$BB mkfifo /etc/ramshared/origin.conf || fail seal_fifo_fixture
expect_startup_refusal fifo_seal "bounded, singly linked sealed regular file"
$BB rm /etc/ramshared/origin.conf || fail seal_fifo_cleanup
$BB cp /tmp/origin.conf.saved /etc/ramshared/origin.conf || fail restore_after_fifo
$BB sed "s/^partition_dev_t=.*/partition_dev_t=$PDP/" /tmp/origin.conf.saved > /etc/ramshared/origin.conf
expect_startup_refusal wrong_partition_identity "opened origin identity differs from the sealed manifest"
$BB cp /tmp/origin.conf.saved /etc/ramshared/origin.conf || fail restore_seal
log "gate_origin_seal_refusals=PASS"
start_daemon 4 || fail legitimate_startup
[ "$($BB sha256sum /proc/$DAEMON_PID/exe | $BB cut -d' ' -f1)" = "$EXPECTED" ] || fail running_binary_mismatch
$BB grep -q 'mode=authoritative-origin' /tmp/daemon.log || fail wrong_serving_mode
/nbd-client -N default -unix /run/nbd.sock /dev/nbd0 >/tmp/nbd.log 2>&1 || fail attach
[ "$($BB blockdev --getsize64 /dev/nbd0)" = 1073741824 ] || fail export_capacity
DAEMON_START=$($BB sed 's/.*) //' /proc/$DAEMON_PID/stat | $BB awk '{print $20}')
SOCKET_DEV=$($BB stat -c %d /run/nbd.sock)
SOCKET_INO=$($BB stat -c %i /run/nbd.sock)
/origincheck write /dev/nbd0 /dev/vdb1 || fail payload_roundtrip
[ "$(header_hash)" = "$HEADER" ] || fail origin_header_overwritten
log "gate_origin_integrity=PASS"
[ "$(rows /dev/nbd0)" = 0 ] || fail tier_unexpectedly_armed
/nbd-client -d /dev/nbd0 >/tmp/detach.log 2>&1 || fail detach
if $BB kill -0 "$DAEMON_PID" 2>/dev/null; then
  $BB kill -TERM "$DAEMON_PID" || fail signal_owned_daemon
fi
i=0
while $BB kill -0 "$DAEMON_PID" 2>/dev/null; do
  [ "$i" -lt 30 ] || fail daemon_stop_timeout
  $BB sleep 1
  i=$((i+1))
done
wait "$DAEMON_PID" || fail daemon_stop_error
[ ! -S /run/nbd.sock ] || fail socket_residue
/origincheck read /dev/vdb1 || fail post_stop_origin_integrity
log "gate_origin_shutdown=PASS"
if [ -f /expected-cli.sha256 ]; then
  EXPECTED_CLI=$($BB cat /expected-cli.sha256)
  [ "${#EXPECTED_CLI}" = 64 ] || fail cli_expected_hash
  [ "$($BB sha256sum /ramshared | $BB cut -d' ' -f1)" = "$EXPECTED_CLI" ] || fail cli_binary_mismatch
  log "gate_cli_binary_match=PASS sha256=$EXPECTED_CLI"
  $BB mkdir -p /run/ramshared || fail cli_runtime_directory
  BOOT=$($BB cat /proc/sys/kernel/random/boot_id)
  NDT=$($BB cat /sys/class/block/nbd0/dev)
  NSP=$($BB readlink -f /sys/class/block/nbd0)
  # This is a lab binding assembled from measured guest identities. It is
  # not an installed activation receipt or Windows ownership qualification.
  printf '{"schema_version":1,"boot_id":"%s","daemon_invocation_id":"0123456789abcdef0123456789abcdef","daemon_pid":%s,"daemon_start_ticks":%s,"daemon_instance_id":"%s-%s","export_socket":{"path":"/run/nbd.sock","filesystem_dev":%s,"inode":%s},"origin":{"partuuid":"%s","ptuuid":"%s","partition_dev_t":"%s","parent_dev_t":"%s","expected_swap_uuid":"%s","host_manifest_sha256":"%s","configuration_sha256":"%s"},"devices":[{"kind":"nbd","path":"/dev/nbd0","dev_t":"%s","sysfs_path":"%s","sysfs_dev_t":"%s","kernel_owner_instance_id":null}]}' \
    "$BOOT" "$DAEMON_PID" "$DAEMON_START" "$DAEMON_PID" "$DAEMON_START" "$SOCKET_DEV" "$SOCKET_INO" \
    "$PUUID" "$PTUUID" "$PDT" "$PDP" "$SUUID" "$HOSTSHA" "$CFGSHA" "$NDT" "$NSP" "$NDT" \
    > /run/ramshared/lifecycle-binding.json || fail cli_binding_fixture
  $BB chmod 600 /run/ramshared/lifecycle-binding.json || fail cli_binding_mode
  $BB cp /run/ramshared/lifecycle-binding.json /tmp/cli-binding.saved || fail cli_binding_save
  printf '%s\n' "$DAEMON_PID" > /run/ramshared/ramsharedd.pid
  $BB rm -f /run/ramshared/cache-status.json || fail cli_cache_fixture_replace
  $BB mkdir /run/ramshared/cache-status.json || fail cli_cleanup_obstruction
  BINDING_HASH=$($BB sha256sum /tmp/cli-binding.saved | $BB cut -d' ' -f1)
  $BB sed "s/\"boot_id\":\"$BOOT\"/\"boot_id\":\"aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee\"/" /tmp/cli-binding.saved > /run/ramshared/lifecycle-binding.json
  /ramshared down --recover-released-peer >/tmp/cli-boot.log 2>&1 && fail cli_boot_false_success
  $BB grep -Fq 'boot identity differs' /tmp/cli-boot.log || fail cli_boot_wrong_refusal
  [ -f /run/ramshared/ramsharedd.pid ] && [ -d /run/ramshared/cache-status.json ] || fail cli_boot_removed_evidence
  $BB cp /tmp/cli-binding.saved /run/ramshared/lifecycle-binding.json || fail cli_binding_restore
  expect_binding_refusal() {
    local label=$1 before
    before=$($BB sha256sum /run/ramshared/lifecycle-binding.json | $BB cut -d' ' -f1)
    /ramshared down --recover-released-peer >/tmp/cli-binding-refusal.log 2>&1 && fail "${label}_false_success"
    $BB grep -Fq 'lifecycle binding is not a sealed regular file' /tmp/cli-binding-refusal.log || fail "${label}_wrong_refusal"
    [ "$($BB sha256sum /run/ramshared/lifecycle-binding.json | $BB cut -d' ' -f1)" = "$before" ] || fail "${label}_changed_binding"
    [ -f /run/ramshared/ramsharedd.pid ] && [ -d /run/ramshared/cache-status.json ] || fail "${label}_removed_evidence"
    log "binding_refusal=$label"
  }
  $BB ln /run/ramshared/lifecycle-binding.json /tmp/cli-binding.alias || fail cli_binding_link_fixture
  expect_binding_refusal hardlinked_binding
  $BB rm /tmp/cli-binding.alias || fail cli_binding_link_cleanup
  $BB chmod 644 /run/ramshared/lifecycle-binding.json || fail cli_binding_mode_fixture
  expect_binding_refusal public_binding
  $BB chmod 600 /run/ramshared/lifecycle-binding.json || fail cli_binding_mode_restore
  printf '%65536s' '' >> /run/ramshared/lifecycle-binding.json || fail cli_binding_growth_fixture
  [ "$($BB stat -c %s /run/ramshared/lifecycle-binding.json)" -gt 65536 ] || fail cli_binding_growth_size
  expect_binding_refusal oversized_binding
  $BB cp /tmp/cli-binding.saved /run/ramshared/lifecycle-binding.json || fail cli_binding_growth_restore
  /ramshared down --recover-released-peer >/tmp/cli-cleanup-first.log 2>&1 && fail cli_cleanup_false_success
  $BB grep -Fq 'runtime cleanup incomplete' /tmp/cli-cleanup-first.log || fail cli_cleanup_wrong_refusal
  [ "$($BB sha256sum /run/ramshared/lifecycle-binding.json | $BB cut -d' ' -f1)" = "$BINDING_HASH" ] || fail cli_cleanup_lost_binding
  [ -f /run/ramshared/ramsharedd.pid ] || fail cli_cleanup_lost_pid
  $BB rmdir /run/ramshared/cache-status.json || fail cli_obstruction_repair
  /ramshared down --recover-released-peer >/tmp/cli-cleanup-retry.log 2>&1 || fail cli_cleanup_retry
  [ ! -e /run/ramshared/lifecycle-binding.json ] && [ ! -e /run/ramshared/ramsharedd.pid ] || fail cli_cleanup_residue
  /origincheck read /dev/vdb1 || fail cli_post_cleanup_integrity
  log "gate_released_cleanup_resume=PASS refusals=boot,hardlink,mode,oversize,unlink retry=complete"
fi
[ "$(rows /dev/vdc1)" = 1 ] || fail final_fallback_missing
[ "$(rows /dev/nbd0)" = 0 ] || fail final_tier_armed
log "gate_origin_fallback=PASS"
if $BB dmesg | $BB grep -Eq 'Kernel panic|WARNING:|BUG:|Oops:|I/O error|Unexpected reply|Out of memory|oom-kill|SIGBUS'; then
  fail kernel_signal
fi
log "gate_origin_kernel_clean=PASS"
log "scenario_done=s12"
$BB poweroff -f
S12
chmod +x "$IRD/s12.sh"

cat > "$IRD/s6.sh" <<'S6'
#!/bin/busybox sh
. /origin_env.sh

# The image files are sparse: reading untouched extents is RAM speed, not
# disk speed. Every measured span is written once first (a 64 MiB warm
# pass), so reads and writes hit real storage in every comparison.
log "fallback_warm $(/rate /dev/vdc1 4 4096 64 w)"
FB_READ=$(/rate /dev/vdc1 4 2048 64 r); log "fallback_read $FB_READ"
log "fallback_write $(/rate /dev/vdc1 4 2048 64 w)"
log "origin_raw_read $(/rate /dev/vdb1 4 2048 64 r)"
start_daemon 4 || { log "aborting"; $BB poweroff -f; }
attach
log "tier_warm $(/rate /dev/nbd0 4 4096 64 w)"
N4_READ=$(/rate /dev/nbd0 4 2048 64 r); log "tier_read_n4 $N4_READ"
log "tier_write_n4 $(/rate /dev/nbd0 4 2048 64 w)"
L4=$(/rate /dev/nbd0 1 512 64 r); log "tier_lat_n4 $L4"
stop_daemon
start_daemon 1 || { log "aborting_n1"; $BB poweroff -f; }
attach
N1_READ=$(/rate /dev/nbd0 4 2048 64 r); log "tier_read_n1 $N1_READ"
L1=$(/rate /dev/nbd0 1 512 64 r); log "tier_lat_n1 $L1"
stop_daemon
N4=$(iops_of "$N4_READ"); N1=$(iops_of "$N1_READ"); FB=$(iops_of "$FB_READ")
P4=$(p50_of "$L4"); P1=$(p50_of "$L1")
log "verdict_pool_gain=$(awk "BEGIN{printf \"%.2f\", $N4/$N1}")"
log "verdict_p50_ratio=$(awk "BEGIN{printf \"%.2f\", $P4/$P1}")"
log "verdict_parity_ratio=$(awk "BEGIN{printf \"%.2f\", $N4/$FB}")"
[ "$N4" -ge "$(awk "BEGIN{printf \"%d\", $N1*1.2}")" ] && log "gate_pool=PASS" || log "gate_pool=FAIL"
[ "$P4" -le "$(awk "BEGIN{printf \"%d\", $P1*1.2}")" ] && log "gate_p50=PASS" || log "gate_p50=FAIL"
[ "$N4" -ge "$FB" ] && log "gate_parity=PASS" || log "gate_parity=FAIL"
log "s6_done"
$BB poweroff -f
S6

cat > "$IRD/s9.sh" <<'S9'
#!/bin/busybox sh
. /origin_env.sh
# The v2 go/no-go (SPEC zram-writeback-vram-tier DT-3): the tier as zram's
# writeback target. Leg A measures the compressed spill (rate, ratio,
# overflow); leg B throttles the tier and asks whether reclaim continues.
# BACKING (ktest.backing) selects the device under test: the product's NBD
# tier by default, a virtio partition for the control run.
HOG_MB=384
RESERVE_KB=32768
ZRAM_SIZE=268435456
WB_LIMIT_PAGES=8192
BACKING=$($BB sed -n 's/.*ktest\.backing=\([^ ]*\).*/\1/p' /proc/cmdline)
[ -n "$BACKING" ] || BACKING=/dev/nbd0
log "s9_backing=$BACKING"
swap_used_kb() { $BB awk -v d="$1" '$1==d{print int($4/1024)}' /proc/swaps; }
# /sys/block/*/stat: 5 write_ios 6 write_merges 7 write_sectors 8 write_ticks
tier_write_kb() { $BB awk '{print int($7*512/1024)}' "$1"; }
STAT=/sys/block/nbd0/stat
[ "$BACKING" = "/dev/vdc1" ] && STAT=/sys/block/vdc/vdc1/stat
zram_mm() { $BB awk -v n="$1" '{print $n}' /sys/block/zram0/mm_stat; }
setup_zram() {
  $BB insmod /modules/zsmalloc.ko 2>/dev/null
  ZERR=$($BB insmod /modules/zram.ko 2>&1); [ -z "$ZERR" ] && log "zram_module=OK" || log "zram_module=FAIL err=$ZERR"
  $BB sleep 1
  echo 1 > /sys/class/zram-control/hot_add 2>/dev/null
  [ -b /dev/zram0 ] && log "zram_hot_add=OK" || { log "zram_hot_add=FAIL"; return 1; }
  # The kernel refuses backing_dev on an initialized device: the product's
  # order is backing_dev -> comp_algorithm -> disksize (DT-2).
  BERR=$($BB sh -c "echo $BACKING > /sys/block/zram0/backing_dev" 2>&1)
  [ -z "$BERR" ] || { log "zram_backing_dev=FAIL err=$BERR"; return 1; }
  log "zram_backing_dev=OK"
  log "zram_alg=$($BB cat /sys/block/zram0/comp_algorithm | $BB sed 's/[][]//g' | $BB cut -d' ' -f1)"
  DERR=$($BB sh -c "echo $ZRAM_SIZE > /sys/block/zram0/disksize" 2>&1)
  log "zram_disksize_err=$DERR"
  echo 1 > /sys/block/zram0/writeback_limit_enable
  echo $WB_LIMIT_PAGES > /sys/block/zram0/writeback_limit
  log "zram_setup=OK size_kb=$((ZRAM_SIZE/1024)) wb_pages=$WB_LIMIT_PAGES"
  return 0
}
arm_swap() {
  $BB mkswap /dev/zram0 >/dev/null 2>&1 && log "zram_mkswap=OK" || log "zram_mkswap=FAIL"
  SERR=$(/swapon_prio /dev/zram0 200 2>&1)
  [ -z "$SERR" ] && log "zram_swapon=OK" || { log "zram_swapon=FAIL err=$SERR"; return 1; }
  if [ "$BACKING" != "/dev/vdc1" ]; then
    SERR=$(/swapon_prio /dev/vdc1 100 2>&1)
    [ -z "$SERR" ] && log "fallback_swapon=OK" || { log "fallback_swapon=FAIL err=$SERR"; return 1; }
  else
    log "fallback_swapon=SKIP (it is the backing device under test)"
  fi
  return 0
}
run_hog() {
  $BB rm -f /tmp/hog.log
  $BB mkdir -p /sys/fs/cgroup/daemon
  $BB sh -c "echo \$\$ > /sys/fs/cgroup/daemon/cgroup.procs; exec /randhog $HOG_MB $((HOG_MB - 64))" >/tmp/hog.log 2>&1 & HOG=$!
}
start_daemon 4 || { log "aborting_s9"; $BB poweroff -f; }
attach || { log "attach_failed"; $BB poweroff -f; }
setup_zram || { log "zram_setup_failed"; log "s9_done"; $BB poweroff -f; }
arm_swap || { log "swap_arm_failed"; log "s9_done"; $BB poweroff -f; }
log "s9_swaps=$($BB cat /proc/swaps | $BB tr '\n' ';')"
log "s9_tier_in_swaps=$($BB grep -c 'nbd0' /proc/swaps)"

# --- Leg A: the spill ---
F0=$(swap_used_kb /dev/vdc1); Z0=$(swap_used_kb /dev/zram0)
run_hog
$BB sleep 25
# The spill is not automatic: this kernel writes to the backing device only
# when the idle pages are marked and the writeback is pumped. The pump's
# window is what the rate measures.
IERR=$($BB sh -c 'echo all > /sys/block/zram0/idle' 2>&1)
W0=$(tier_write_kb "$STAT")
P0=$($BB cut -d' ' -f1 /proc/uptime)
WERR=$($BB sh -c 'echo idle > /sys/block/zram0/writeback' 2>&1)
P1=$($BB cut -d' ' -f1 /proc/uptime)
WERR=${WERR#sh: write error: }
$BB sleep 2
W1=$(tier_write_kb "$STAT"); F1=$(swap_used_kb /dev/vdc1); Z1=$(swap_used_kb /dev/zram0)
ORIG=$(zram_mm 1); COMPR=$(zram_mm 2)
SPILL_MB=$(( (W1 - W0) / 1024 )); SPILL_S=$(awk "BEGIN{printf \"%.2f\", $P1-$P0}")
log "s9_spill_tier_mb=$SPILL_MB"
log "s9_spill_pump_s=$SPILL_S"
log "s9_zram_used_mb=$(( (Z1 - Z0) / 1024 )) overflow_mb=$(( (F1 - F0) / 1024 ))"
log "s9_compression_ratio=$(awk "BEGIN{printf \"%.2f\", ($ORIG>0)?$COMPR/$ORIG:0}") orig_kb=$((ORIG/1024)) compr_kb=$((COMPR/1024))"
log "s9_spill_mbs=$(awk "BEGIN{printf \"%.1f\", ($SPILL_S>0)?$SPILL_MB/$SPILL_S:0}")"
[ "$SPILL_MB" -gt 0 ] && log "s9_spill_observed=YES" || log "s9_spill_observed=NO"
# The EIO is the one-shot writeback budget's exhaustion signal (the kernel
# returns -EIO when bd_wb_limit reaches 0), not an I/O error.
case "$WERR" in
  "") log "s9_writeback=OK" ;;
  *"Input/output error"*) log "s9_writeback=budget-exhausted (one-shot writeback_limit)" ;;
  *) log "s9_writeback=FAIL err=$WERR" ;;
esac
log "s9_hog=$($BB cat /tmp/hog.log 2>/dev/null | $BB tr '\n' ';')"
[ "$SPILL_MB" -gt 0 ] && [ "$(awk "BEGIN{print ($SPILL_S>0 && $SPILL_MB/$SPILL_S < 29) ? 1 : 0}")" = "1" ] && log "gate_spill_under_tier_rate=PASS" || log "gate_spill_under_tier_rate=FAIL"
case "$WERR" in
  "") log "gate_writeback=OK" ;;
  *"Input/output error"*) log "gate_writeback=BUDGET (not an I/O error)" ;;
  *) log "gate_writeback=FAIL err=$WERR" ;;
esac
$BB kill $HOG 2>/dev/null
$BB sleep 2
$BB swapoff /dev/zram0 >/dev/null 2>&1

# --- Leg B: the tier throttled — does reclaim continue? ---
$BB swapoff /dev/vdc1 >/dev/null 2>&1
[ "$BACKING" != "/dev/vdc1" ] && /swapon_prio /dev/vdc1 100 2>/dev/null && log "fallback_reswapon=OK"
$BB mkdir -p /sys/fs/cgroup/daemon
$BB sh -c 'for p in $(pidof ramsharedd); do echo $p > /sys/fs/cgroup/daemon/cgroup.procs; done' 2>/dev/null
echo 10000 100000 > /sys/fs/cgroup/daemon/cpu.max 2>/dev/null && log "daemon_throttled=OK" || log "daemon_throttled=SKIP"
T0=$($BB cut -d' ' -f1 /proc/uptime)
run_hog
PROGRESS=NO
i=0
while [ $i -lt 45 ]; do
  $BB sleep 1
  if $BB grep -q "holding" /tmp/hog.log 2>/dev/null; then PROGRESS=YES; break; fi
  i=$((i+1))
done
T1=$($BB cut -d' ' -f1 /proc/uptime)
log "s9_slow_tier_progress=$PROGRESS elapsed_s=$(awk "BEGIN{printf \"%.1f\", $T1-$T0}") overflow_mb=$(( ( $(swap_used_kb /dev/vdc1) - F1 ) / 1024 )) psi=$(awk '/^some/{print $2}' /proc/pressure/memory 2>/dev/null)"
[ "$PROGRESS" = "YES" ] && log "gate_slow_tier_continues=PASS" || log "gate_slow_tier_continues=FAIL"
$BB kill $HOG 2>/dev/null
echo 0 > /sys/fs/cgroup/daemon/cpu.max 2>/dev/null
stop_daemon
# Round 4: the zram writeback starts at the backing device's block 0 — the
# origin's swap-signature page. A daemon restart after a spill must still
# pass the seal check; if it does not, the spill clobbered the sealed
# identity.
start_daemon 4 >/dev/null 2>&1 && log "s9_restart_after_spill=OK (the seal survived)" || log "s9_restart_after_spill=FAIL (the spill clobbered the seal page)"
stop_daemon
log "s9_done"
$BB poweroff -f
S9
cat > "$IRD/s11.sh" <<'S11'
#!/bin/busybox sh
. /origin_env.sh
# The product's own spill pump, end to end: the daemon runs with
# --zram-pump-tier (the code the slice implemented), the zram is backed by
# the tier, the pressure spills, and the pump's ticks do the work — no
# manual sysfs writes in the drill.
HOG_MB=256
OUT_MB=192
ZRAM_SIZE=134217728
swap_used_kb() { $BB awk -v d="$1" '$1==d{print int($4/1024)}' /proc/swaps; }
tier_write_kb() { $BB awk '{print int($7*512/1024)}' "$1"; }
start_daemon_with_pump 4 /dev/nbd0 || { log "aborting_s11"; $BB poweroff -f; }
attach || { log "attach_failed"; $BB poweroff -f; }
$BB insmod /modules/zsmalloc.ko 2>/dev/null
$BB insmod /modules/zram.ko 2>/dev/null
$BB sleep 1
echo 1 > /sys/class/zram-control/hot_add 2>/dev/null
[ -b /dev/zram0 ] && log "s11_zram_hot_add=OK" || { log "s11_zram_hot_add=FAIL"; $BB poweroff -f; }
BERR=$($BB sh -c 'echo /dev/nbd0 > /sys/block/zram0/backing_dev' 2>&1)
[ -z "$BERR" ] || { log "s11_backing_dev=FAIL err=$BERR"; $BB poweroff -f; }
echo $ZRAM_SIZE > /sys/block/zram0/disksize
$BB mkswap /dev/zram0 >/dev/null 2>&1
SERR=$(/swapon_prio /dev/zram0 200 2>&1)
[ -z "$SERR" ] && log "s11_zram_swapon=OK" || { log "s11_zram_swapon=FAIL err=$SERR"; $BB poweroff -f; }
SERR=$(/swapon_prio /dev/vdc1 100 2>&1)
[ -z "$SERR" ] && log "s11_fallback_swapon=OK" || log "s11_fallback_swapon=FAIL err=$SERR"
$BB rm -f /tmp/hog.log
$BB sh -c "exec /randhog $HOG_MB $OUT_MB" >/tmp/hog.log 2>&1 & HOG=$!
# The pump ticks every 5 s; the pressure needs the zram's pool to overflow
# so the writeback has something to spill.
$BB sleep 25
SPILLED=$(tier_write_kb /sys/block/nbd0/stat)
log "s11_spilled_kb=$SPILLED zram_used_kb=$(swap_used_kb /dev/zram0) fallback_kb=$(swap_used_kb /dev/vdc1)"
log "s11_daemon_pump_lines=$($BB grep 'spill pump' /tmp/daemon.log 2>/dev/null | $BB tr '\n' ';')"
[ "${SPILLED:-0}" -gt 0 ] && log "gate_product_pump_spills=PASS" || log "gate_product_pump_spills=FAIL"
$BB kill -9 $HOG 2>/dev/null
log "s11_done"
$BB poweroff -f
S11
cat > "$IRD/s10.sh" <<'S10'
#!/bin/busybox sh
. /origin_env.sh
# ITEM-4: the SIGBUS exposure of the writeback topology, measured. The
# daemon (the tier) dies while pages are spilled; the hot pages (in the
# zram's RAM) must survive, and the spilled pages (on the dead tier) are
# the bounded exposure. The verdict is the exposure class, not a rate.
HOG_MB=256
OUT_MB=128
ZRAM_SIZE=268435456
WB_LIMIT_PAGES=8192
swap_used_kb() { $BB awk -v d="$1" '$1==d{print int($4/1024)}' /proc/swaps; }
tier_write_kb() { $BB awk '{print int($7*512/1024)}' "$1"; }
zram_mm() { $BB awk -v n="$1" '{print $n}' /sys/block/zram0/mm_stat; }
setup_zram() {
  $BB insmod /modules/zsmalloc.ko 2>/dev/null
  $BB insmod /modules/zram.ko 2>/dev/null
  $BB sleep 1
  echo 1 > /sys/class/zram-control/hot_add 2>/dev/null
  [ -b /dev/zram0 ] && log "zram_hot_add=OK" || { log "zram_hot_add=FAIL"; return 1; }
  BERR=$($BB sh -c 'echo /dev/nbd0 > /sys/block/zram0/backing_dev' 2>&1)
  [ -z "$BERR" ] || { log "zram_backing_dev=FAIL err=$BERR"; return 1; }
  echo $ZRAM_SIZE > /sys/block/zram0/disksize
  echo 1 > /sys/block/zram0/writeback_limit_enable
  echo $WB_LIMIT_PAGES > /sys/block/zram0/writeback_limit
  $BB mkswap /dev/zram0 >/dev/null 2>&1
  SERR=$(/swapon_prio /dev/zram0 200 2>&1)
  [ -z "$SERR" ] && log "zram_swapon=OK" || { log "zram_swapon=FAIL err=$SERR"; return 1; }
  SERR=$(/swapon_prio /dev/vdc1 100 2>&1)
  [ -z "$SERR" ] && log "fallback_swapon=OK" || log "fallback_swapon=FAIL err=$SERR"
  return 0
}
start_daemon 4 || { log "aborting_s10"; $BB poweroff -f; }
attach || { log "attach_failed"; $BB poweroff -f; }
setup_zram || { log "zram_setup_failed"; log "s10_done"; $BB poweroff -f; }
$BB rm -f /tmp/hog.log
$BB sh -c "exec /randhog $HOG_MB $OUT_MB" >/tmp/hog.log 2>&1 & HOG=$!
$BB sleep 15
# The pump: mark idle, arm the budget, trigger the writeback.
$BB sh -c 'echo all > /sys/block/zram0/idle' 2>/dev/null
$BB sh -c 'echo idle > /sys/block/zram0/writeback' 2>/dev/null
$BB sleep 3
SPILLED=$(tier_write_kb /sys/block/nbd0/stat)
log "s10_spilled_kb=$SPILLED"
log "s10_zram_used_kb=$(swap_used_kb /dev/zram0) orig_kb=$(zram_mm 1) compr_kb=$(zram_mm 2)"
# The tier dies: the daemon is killed, so the NBD device goes away.
$BB kill -9 $(pidof ramsharedd) 2>/dev/null
$BB sleep 3
log "s10_daemons_after_kill=$($BB pidof ramsharedd 2>/dev/null | wc -w)"
log "s10_tier_state=$($BB cat /sys/block/nbd0/stat 2>/dev/null | $BB awk '{print $8}')"
# The hot region must survive the dead tier.
$BB kill -USR1 $HOG
$BB sleep 3
if $BB grep -q hot_touch /tmp/hog.log 2>/dev/null; then
  log "s10_hot_survived=YES"
else
  log "s10_hot_survived=NO"
fi
# The spilled region is the bounded exposure: the fault must fail.
$BB kill -USR2 $HOG
$BB sleep 3
if $BB grep -q spilled_touch /tmp/hog.log 2>/dev/null; then
  log "s10_spilled_touch=COMPLETED (the exposure did not bite)"
  log "gate_exposure_bounded=FAIL"
else
  log "s10_spilled_touch=FAILED_AS_EXPECTED (the spilled pages are the exposure)"
  log "gate_exposure_bounded=PASS"
fi
log "s10_hog_alive=$($BB kill -0 $HOG 2>/dev/null && echo yes || echo no)"
$BB kill -9 $HOG 2>/dev/null
log "s10_done"
$BB poweroff -f
S10
cat > "$IRD/s7.sh" <<'S7'
#!/bin/busybox sh
. /origin_env.sh
# The swap-path leg (SPEC wsl2-nbd-serving-throughput ITEM-4's parity
# question, the verified test case shape): tier vs fallback as swap devices. The
# clean swap-cache copies are evicted with the cgroup's memory.reclaim,
# which drops clean pages without generating writeback, so the touch that
# follows reads the device instead of queueing behind a post-pressure
# backlog (the s6 attempt's failure).
HOG_MB=128
swap_cached_kb() { $BB awk '/SwapCached/{print $2}' /proc/meminfo; }
wait_used() {
  local dev=$1 target=$2 secs=$3 i=0 used=""
  while [ $i -lt "$secs" ]; do
    used=$($BB awk -v d="$dev" '$1==d{print int($4/1024)}' /proc/swaps)
    [ -n "$used" ] && [ "$used" -ge "$target" ] && { echo "$i"; return 0; }
    $BB sleep 1; i=$((i+1))
  done
  echo "timeout"; return 1
}
wait_cache_drop() {
  local target=$1 secs=$2 i=0 c=""
  while [ $i -lt "$secs" ]; do
    c=$(swap_cached_kb)
    [ -n "$c" ] && [ "$c" -lt "$target" ] && { echo "$c"; return 0; }
    $BB sleep 1; i=$((i+1))
  done
  echo "timeout:$c"; return 1
}
touches() { $BB grep -c touch_ms /tmp/hog.log 2>/dev/null || echo 0; }
wait_touch() {
  local want=$1 secs=$2 i=0
  while [ $i -lt "$secs" ]; do
    if [ "$(touches)" -ge "$want" ]; then return 0; fi
    $BB sleep 1; i=$((i+1))
  done
  return 1
}
last_touch() { $BB grep touch_ms /tmp/hog.log | $BB tail -1; }
mbs_of() { echo "$1" | $BB sed 's/.*mbs=\([0-9.]*\).*/\1/'; }
reclaim_cache() {
  # memory.reclaim drops the cgroup's clean swap-cache copies with no I/O.
  echo 256M > /sys/fs/cgroup/swaphog/memory.reclaim 2>/dev/null
}
start_daemon 4 || { log "aborting_swap"; $BB poweroff -f; }
attach
$BB mkdir -p /sys/fs/cgroup/swaphog
echo 536870912 > /sys/fs/cgroup/swaphog/memory.max
$BB mkswap /dev/vdc1 >/dev/null 2>&1
$BB swapon /dev/vdc1 && log "fallback_swapon=OK" || log "fallback_swapon=FAIL"
$BB rm -f /tmp/hog.log
$BB sh -c "echo \$\$ > /sys/fs/cgroup/swaphog/cgroup.procs; exec /swaphog $HOG_MB 0" >/tmp/hog.log 2>&1 & HOG=$!
log "fallback_swapout_s=$(wait_used /dev/vdc1 120 90)"
log "fallback_swap_cached_kb=$(swap_cached_kb)"
reclaim_cache
log "fallback_swap_cached_evicted_kb=$(wait_cache_drop 8192 60)"
$BB kill -USR1 $HOG
wait_touch 1 120 && FB_TOUCH="$(last_touch)" || FB_TOUCH=""
log "fallback_swapin ${FB_TOUCH:-timeout}"
log "fallback_swap_cached_after_kb=$(swap_cached_kb)"
$BB swapoff /dev/vdc1 >/dev/null 2>&1 && log "fallback_swapoff=OK"
# The tier's swap signature is the origin's own (the daemon serves the
# partition; an mkswap on the tier would overwrite the origin's header
# through the write-through and the next seal check would refuse it).
$BB swapon /dev/nbd0 && log "tier_swapon=OK" || log "tier_swapon=FAIL"
$BB kill -USR2 $HOG
log "tier_swapout_s=$(wait_used /dev/nbd0 120 150)"
log "tier_swap_cached_kb=$(swap_cached_kb)"
reclaim_cache
log "tier_swap_cached_evicted_kb=$(wait_cache_drop 8192 90)"
$BB kill -USR1 $HOG
wait_touch 2 180 && T4_TOUCH="$(last_touch)" || T4_TOUCH=""
log "tier_swapin ${T4_TOUCH:-timeout}"
log "tier_swap_cached_after_kb=$(swap_cached_kb)"
$BB swapoff /dev/nbd0 >/dev/null 2>&1 && log "tier_swapoff=OK"
# The pool's value on the swap path: the same round with N = 1.
stop_daemon
start_daemon 1 || { log "aborting_n1"; $BB poweroff -f; }
attach
$BB swapon /dev/nbd0 && log "tier_n1_swapon=OK" || log "tier_n1_swapon=FAIL"
$BB kill -USR2 $HOG
log "tier_n1_swapout_s=$(wait_used /dev/nbd0 120 150)"
reclaim_cache
log "tier_n1_swap_cached_evicted_kb=$(wait_cache_drop 8192 90)"
$BB kill -USR1 $HOG
wait_touch 3 240 && T1_TOUCH="$(last_touch)" || T1_TOUCH=""
log "tier_n1_swapin ${T1_TOUCH:-timeout}"
$BB swapoff /dev/nbd0 >/dev/null 2>&1 && log "tier_n1_swapoff=OK"
$BB kill $HOG 2>/dev/null
stop_daemon
FBS=$(mbs_of "$FB_TOUCH"); T4S=$(mbs_of "$T4_TOUCH"); T1S=$(mbs_of "$T1_TOUCH")
log "s7_verdicts fallback_mbs=$FBS tier_n4_mbs=$T4S tier_n1_mbs=$T1S"
log "s7_pool_gain_swap=$(awk "BEGIN{printf \"%.2f\", $T4S/$T1S}")"
log "s7_parity_swap=$(awk "BEGIN{printf \"%.2f\", $T4S/$FBS}")"
[ "$(awk "BEGIN{print ($T4S >= $T1S) ? 1 : 0}")" = "1" ] && log "gate_swap_pool=PASS" || log "gate_swap_pool=FAIL"
[ "$(awk "BEGIN{print ($T4S >= $FBS) ? 1 : 0}")" = "1" ] && log "gate_swap_parity=PASS" || log "gate_swap_parity=FAIL"
log "s7_done"
$BB poweroff -f
S7
chmod +x "$IRD/s7.sh"
chmod +x "$IRD/s6.sh"

# Copies dynamic libraries while preserving absolute paths (the daemon,
# nbd-client and blkid are glibc-dynamic; the linker is included as well).
# blkid is the daemon's device-identity authority (PARTUUID/UUID/TYPE), so
# the guest needs the real binary, not a substitute.
BLKID=$(command -v blkid)
[ -n "$BLKID" ] || { echo "blkid missing" >&2; exit 2; }
mkdir -p "$IRD$(dirname "$BLKID")"
cp "$BLKID" "$IRD$BLKID"
for bin in "$DAEMON" "$NBD_CLIENT" "$BLKID" ${RECOVERY_CLI:+"$RECOVERY_CLI"} ${EMERGENCY_TEST:+"$EMERGENCY_TEST"} "${EMERGENCY_TOOLS[@]}"; do
  for lib in $(ldd "$bin" | grep -oE '/[^ ]+\.so[^ ]*'); do
    mkdir -p "$IRD$(dirname "$lib")"
    cp "$lib" "$IRD$lib"
  done
done

cat > "$IRD/init" <<'INIT'
#!/bin/busybox sh
BB=/bin/busybox
$BB mkdir -p /proc /sys /dev /run /tmp
$BB mount -t proc proc /proc
$BB mount -t sysfs sysfs /sys
$BB mount -t devtmpfs devtmpfs /dev 2>/dev/null
SCENARIO=$($BB sed -n 's/.*ktest.scenario=\([a-z0-9]*\).*/\1/p' /proc/cmdline)
boot_fail() {
  echo "KTEST scenario_failed=$SCENARIO reason=$1"
  $BB poweroff -f
  exit 1
}
if [ "$SCENARIO" = "s6" ] || [ "$SCENARIO" = "s7" ] || [ "$SCENARIO" = "s9" ] || [ "$SCENARIO" = "s10" ] || [ "$SCENARIO" = "s11" ] || [ "$SCENARIO" = "s12" ] || [ "$SCENARIO" = "s19" ] || [ "$SCENARIO" = "s20" ]; then
  $BB insmod /modules/virtio_blk.ko 2>/dev/null && echo "KTEST virtio_blk=OK" || echo "KTEST virtio_blk=FAIL"
  $BB insmod /modules/nbd.ko 2>/dev/null && echo "KTEST nbd_module=OK" || echo "KTEST nbd_module=FAIL"
  $BB mkdir -p /sys/fs/cgroup
  $BB mount -t cgroup2 none /sys/fs/cgroup 2>/dev/null && echo "KTEST cgroup2=OK" || echo "KTEST cgroup2=FAIL"
  echo +memory > /sys/fs/cgroup/cgroup.subtree_control 2>/dev/null || true
  $BB sleep 1
  $BB mkdir -p /newroot
  $BB mount -t ext4 /dev/vda1 /newroot || boot_fail root_mount
  echo "KTEST root_mount=OK"
  for directory in /bin /usr /lib /lib64 /sbin; do
    if [ -d "$directory" ]; then
      $BB cp -a "$directory" /newroot/ || boot_fail runtime_copy
    fi
  done
  $BB cp /ramsharedd /nbd-client /rate /swaphog /swapon_prio /memhog /randhog /origincheck /expected-ramsharedd.sha256 /origin_env.sh /s6.sh /s7.sh /s9.sh /s10.sh /s11.sh /s12.sh /newroot/ || boot_fail scenario_copy
  if [ -f /expected-cli.sha256 ]; then
    $BB cp /ramshared /expected-cli.sha256 /newroot/ || boot_fail cli_copy
  fi
  if [ "$SCENARIO" = s19 ] || [ "$SCENARIO" = s20 ]; then
    $BB cp /s19.sh /s20.sh /dual_common.sh /product-dual-attach /directread /pagetest /pagetrack /newroot/ || boot_fail product_dual_copy
  fi
  $BB cp -a /modules /newroot/modules || boot_fail module_copy
  $BB mkdir -p /newroot/proc /newroot/sys /newroot/dev || boot_fail root_directories
  $BB mount --move /proc /newroot/proc || boot_fail proc_move
  $BB mount --move /sys /newroot/sys || boot_fail sys_move
  $BB mount --move /dev /newroot/dev || boot_fail dev_move
  exec $BB switch_root /newroot /bin/busybox sh /$SCENARIO.sh
fi
$BB mkdir -p /sys/fs/cgroup
$BB mount -t cgroup2 none /sys/fs/cgroup 2>/dev/null && echo "KTEST cgroup2=OK" || echo "KTEST cgroup2=FAIL"
echo "KTEST cgroup_controllers=$($BB cat /sys/fs/cgroup/cgroup.controllers 2>/dev/null)"
echo +memory > /sys/fs/cgroup/cgroup.subtree_control 2>/dev/null || true
$BB mkdir -p /sys/fs/cgroup/hog 2>/dev/null
echo "KTEST cgroup_hog_files=$($BB ls /sys/fs/cgroup/hog 2>/dev/null | $BB tr "\n" ",")"
echo "=====KTEST-BEGIN====="
SCENARIO=$($BB sed -n 's/.*ktest.scenario=\([a-z0-9]*\).*/\1/p' /proc/cmdline)
echo "KTEST scenario=$SCENARIO"
echo "KTEST uname=$($BB uname -r)"
echo "KTEST daemon_sha=$($BB sha256sum /ramsharedd | $BB awk '{print $1}')"
echo "KTEST expected_sha=$($BB cat /expected-ramsharedd.sha256)"
$BB insmod /modules/nbd.ko nbds_max=16 && echo "KTEST nbd_module=OK" || echo "KTEST nbd_module=FAIL"

log() { echo "KTEST $*"; }
abort_scenario() {
  log "scenario_failed=$SCENARIO reason=$1"
  log "failed_peer_log_begin"
  $BB tail -n 40 /tmp/peer.log 2>/dev/null
  log "failed_peer_log_end"
  log "failed_pressure_log_begin"
  $BB tail -n 10 /tmp/hog.log 2>/dev/null
  log "failed_pressure_log_end"
  log "failed_kernel_log_begin"
  $BB dmesg | $BB tail -n 30
  log "failed_kernel_log_end"
  $BB poweroff -f
  exit 1
}
daemon_pids() { $BB pidof ramsharedd 2>/dev/null || true; }
# Anti-self-swap assertion (SPEC tier-architecture-v2 DT-4, verified test case): at
# steady state under pressure the daemon that serves the tier must have no
# swapped pages. Sampled after the pressure has been armed and the re-lock
# interval has passed; a nonzero sample is a failure, not a flake.
daemon_swapped_kb() {
  local total=0
  for pid in $(daemon_pids); do
    kb=$($BB awk '/^Swap:/{s+=$2} END{print s+0}' /proc/$pid/smaps 2>/dev/null)
    total=$((total + ${kb:-0}))
  done
  echo "$total"
}
swap_used_kb() { $BB awk '/nbd0/{print $4}' /proc/swaps; }
nbd_errors() { $BB dmesg | $BB grep -c 'I/O error.*nbd0'; }

# Run a command with a wall-clock bound; report TIMEOUT when it does not
# return. A stuck swap read is uninterruptible, so the command is left
# running and this returns — the boot's work is done either way.
bounded() {
  local secs=$1; shift
  "$@" & local p=$!
  local i=0
  while [ "$i" -lt "$secs" ]; do
    if ! $BB kill -0 "$p" 2>/dev/null; then
      wait "$p"; return $?
    fi
    $BB sleep 1; i=$((i+1))
  done
  echo TIMEOUT
  return 124
}

start_peer() {
  [ -z "$(daemon_pids)" ] || abort_scenario existing_peer
  $BB rm -f /run/nbd.sock
  /ramsharedd --backend ram --slices 1 --slice-mb 256 \
    --sock /run/nbd.sock --arbiter-listen 127.0.0.1:10809 >/tmp/peer.log 2>&1 &
  PEER_PID=$!
  $BB sleep 2
  if $BB kill -0 "$PEER_PID" 2>/dev/null; then
    log "peer_start=OK pids=$PEER_PID"
    # The peer must not be the OOM victim: its death is a scenario, not an
    # accident of the pressure leg.
    echo -1000 > /proc/$PEER_PID/oom_score_adj 2>/dev/null
  else
    log "peer_start=FAIL"; $BB cat /tmp/peer.log
    abort_scenario peer_start_failed
  fi
}

connect_and_swap() {
  /nbd-client -N s0 -unix /run/nbd.sock /dev/nbd0 >/tmp/nbd.log 2>&1
  local rc=$?
  log "nbd_connect=$rc"
  [ "$rc" = 0 ] || abort_scenario nbd_connect_failed
  $BB mkswap /dev/nbd0 >/dev/null 2>&1 && log "mkswap=OK" || abort_scenario mkswap_failed
  $BB swapon /dev/nbd0 2>/tmp/swapon.log && log "swapon=OK" || { log "swapon=FAIL"; $BB cat /tmp/swapon.log; abort_scenario swapon_failed; }
}

arm_pressure() {
  # The hog runs inside a memory cgroup whose limit is below its own
  # footprint: reclaim is contained there, evicting the clean swap-cache
  # copies (their swap slots remain) without starving the peer of memory —
  # the guest-wide exhaustion path livelocks instead.
  $BB mkdir -p /sys/fs/cgroup/hog
  echo 200000000 > /sys/fs/cgroup/hog/memory.max
  # The process must enter the cgroup before it allocates: moving it after
  # leaves the already-charged pages in the root cgroup, where the limit
  # cannot reach them.
  $BB sh -c 'echo $$ > /sys/fs/cgroup/hog/cgroup.procs; exec /memhog 400 32 0' >/tmp/hog.log 2>&1 & HOG=$!
  local i=0
  local ready=0
  # Teardown must not race a pressure producer that is still allocating and
  # paging out. The helper emits its marker only after reaching its hold loop.
  while [ "$i" -lt 90 ]; do
    $BB sleep 1
    log "hog_tick=$i swap_kb=$(swap_used_kb) daemons=$(daemon_pids)"
    i=$((i+1))
    $BB kill -0 "$HOG" 2>/dev/null || abort_scenario pressure_producer_exited
    if $BB grep -q '^allocated 400 MB, paged out 32 MB, post 0 MB, holding$' /tmp/hog.log; then
      ready=1
      break
    fi
  done
  [ "$ready" = 1 ] || abort_scenario pressure_not_ready
  log "gate_pressure_prepared=PASS"
  log "io_errors=$($BB dmesg | $BB grep -c 'I/O error.*nbd0')"
  log "pagetest=$($BB sh -c '/pagetest 2>&1 | tr "\n" ";"')"
  log "swap_cached_before_drop_kb=$($BB awk '/SwapCached/{print $2}' /proc/meminfo)"
  # Clean swap-cache copies are page-cache pages; dropping them forces any
  # later swap-in to read the peer instead of RAM.
  echo 3 > /proc/sys/vm/drop_caches
  $BB sleep 2
  log "swap_cached_after_drop_kb=$($BB awk '/SwapCached/{print $2}' /proc/meminfo)"
  log "mem_free_kb=$($BB awk '/MemFree/{print $2}' /proc/meminfo)"
}

report_direct_read() {
  local label=$1 secs=$2
  local start end rc
  start=$($BB cut -d' ' -f1 /proc/uptime)
  rc=$(bounded "$secs" /directread /dev/nbd0 >/tmp/dr.out 2>&1; echo $?)
  end=$($BB cut -d' ' -f1 /proc/uptime)
  log "${label}_rc=$rc elapsed_s=$($BB awk "BEGIN{printf \"%.1f\", $end-$start}")"
  log "${label}_out=$($BB tr '\n' ';' </tmp/dr.out)"
  return "$rc"
}

report_swapoff() {
  local label=$1 secs=$2 mode=${3:-release}
  [ "$mode" = hold ] || echo max > /sys/fs/cgroup/hog/memory.max 2>/dev/null || true
  local start end rc
  start=$($BB cut -d' ' -f1 /proc/uptime)
  rc=$(bounded "$secs" $BB swapoff /dev/nbd0 >/tmp/so.out 2>&1; echo $?)
  end=$($BB cut -d' ' -f1 /proc/uptime)
  log "${label}_rc=$rc elapsed_s=$($BB awk "BEGIN{printf \"%.1f\", $end-$start}")"
  log "${label}_swap_rows=$($BB grep -c nbd0 /proc/swaps)"
  [ -s /tmp/so.out ] && log "${label}_out=$($BB tr '\n' ';' </tmp/so.out)"
  return "$rc"
}

# Check every byte held by the pressure producer; mere process survival is not integrity.
verify_hog() {
  local gate=$1 i=0 before count latest
  before=$($BB awk '/^full_verify bytes=/{n++} END{print n+0}' /tmp/hog.log)
  if ! $BB kill -USR2 "$HOG" 2>/dev/null; then
    log "gate_${gate}=FAIL"
    return 1
  fi
  while [ "$i" -lt 120 ]; do
    count=$($BB awk '/^full_verify bytes=/{n++} END{print n+0}' /tmp/hog.log)
    if [ "$count" -gt "$before" ]; then
      latest=$($BB awk '/^full_verify bytes=/{line=$0} END{print line}' /tmp/hog.log)
      if [ "$latest" = 'full_verify bytes=419430400 mismatches=0' ] && $BB kill -0 "$HOG" 2>/dev/null; then
        log "${gate}_verification_sequence=$count"
        log "gate_${gate}=PASS bytes=419430400 mismatches=0"
        return 0
      fi
      log "gate_${gate}=FAIL"
      return 1
    fi
    if ! $BB kill -0 "$HOG" 2>/dev/null; then
      log "gate_${gate}=FAIL"
      return 1
    fi
    $BB sleep 1; i=$((i+1))
  done
  log "gate_${gate}=UNVERIFIED"
  return 1
}

echo "===== scenario $SCENARIO ====="
case "$SCENARIO" in
  s1)
    start_peer; connect_and_swap; arm_pressure
    log "s1_tier_live_rows=$($BB grep -c nbd0 /proc/swaps)"
    SWAPPED=$(daemon_swapped_kb)
    log "daemon_swapped_kb=$SWAPPED"
    [ "$SWAPPED" = "0" ] && log "gate_daemon_unswapped=PASS" || log "gate_daemon_unswapped=FAIL"
    ;;
  s2)
    start_peer; connect_and_swap; arm_pressure
    log "s2_used_before_kill_kb=$(swap_used_kb)"
    $BB kill -9 "$(daemon_pids)" 2>/dev/null
    $BB sleep 1
    log "s2_daemons_after_kill=$(daemon_pids)"
    s2_read_ok=0
    report_direct_read s2_read 15 && s2_read_ok=1
    # The teardown measurement runs with the hog alive and its pages truly
    # in swap, so swapoff must read them from the dead peer. The touch that
    # kills the hog comes after: a dead hog's swap entries are freed without
    # reads, which would empty the swap before the measurement.
    s2_swapoff_ok=0
    report_swapoff s2_swapoff 150 && s2_swapoff_ok=1
    log "s2_used_after_swapoff_kb=$(swap_used_kb)"
    if [ "$s2_read_ok" = 1 ] && [ "$s2_swapoff_ok" = 1 ]; then
      verify_hog peer_death_integrity || true
    else
      log "gate_peer_death_integrity=FAIL"
    fi
    $BB kill "$HOG" 2>/dev/null
    ;;
  s3)
    start_peer; connect_and_swap; arm_pressure
    $BB kill -STOP "$(daemon_pids)" 2>/dev/null
    log "s3_stopped=OK"
    # 35 s: past the kernel's first 30 s `Possible stuck request` report.
    report_direct_read s3_read_stopped 35
    log "s3_stuck_lines=$($BB dmesg | $BB grep -c 'Possible stuck request')"
    $BB kill -CONT "$(daemon_pids)" 2>/dev/null
    s3_read_ok=0; s3_swapoff_ok=0
    report_direct_read s3_read_after_cont 30 && s3_read_ok=1
    report_swapoff s3_swapoff 90 && s3_swapoff_ok=1
    if [ "$s3_read_ok" = 1 ] && [ "$s3_swapoff_ok" = 1 ]; then
      verify_hog peer_stall_integrity || true
    else
      log "gate_peer_stall_integrity=FAIL"
    fi
    ;;
  s4)
    start_peer; connect_and_swap; arm_pressure
    $BB kill -TERM "$(daemon_pids)" 2>/dev/null
    $BB sleep 3
    log "s4_daemons_after_term=$(daemon_pids)"
    report_direct_read s4_read 15
    report_swapoff s4_swapoff 150
    ;;
  s5)
    start_peer; connect_and_swap; arm_pressure
    OLD_PEER=$PEER_PID
    report_swapoff s5_swapoff_clean 90 || abort_scenario swapoff_failed
    rows=$($BB grep -c nbd0 /proc/swaps)
    log "s5_rows_after_clean_swapoff=$rows"
    [ "$rows" = 0 ] || abort_scenario swap_still_armed
    verify_hog s5_drain_integrity || abort_scenario drained_data_integrity_failed
    /nbd-client -d /dev/nbd0 >/tmp/detach.log 2>&1 || abort_scenario detach_failed
    i=0
    while [ -n "$($BB cat /sys/block/nbd0/pid 2>/dev/null)" ] && [ "$i" -lt 10 ]; do
      $BB sleep 1; i=$((i+1))
    done
    [ -z "$($BB cat /sys/block/nbd0/pid 2>/dev/null)" ] || abort_scenario detach_unproven
    $BB kill -TERM "$OLD_PEER" 2>/dev/null || true
    i=0
    while $BB kill -0 "$OLD_PEER" 2>/dev/null && [ "$i" -lt 10 ]; do
      $BB sleep 1; i=$((i+1))
    done
    $BB kill -0 "$OLD_PEER" 2>/dev/null && abort_scenario old_peer_still_alive
    start_peer
    [ "$PEER_PID" != "$OLD_PEER" ] || abort_scenario peer_identity_not_changed
    connect_and_swap
    report_direct_read s5_read_after_reconnect 15 || abort_scenario reconnect_read_failed
    log "s5_rows=$($BB grep -c nbd0 /proc/swaps)"
    log "gate_reconnect=PASS old_peer=$OLD_PEER new_peer=$PEER_PID"
    ;;
  s16|s17|s18)
    $BB insmod /modules/virtio_blk.ko 2>/dev/null || abort_scenario emergency_disk_driver_failed
    i=0
    while [ ! -b /dev/vda ] && [ "$i" -lt 10 ]; do $BB sleep 1; i=$((i+1)); done
    [ -b /dev/vda ] || abort_scenario emergency_disk_missing
    $BB mkdir -p /mnt/emergency
    $BB mount -t ext4 /dev/vda /mnt/emergency || abort_scenario emergency_mount_failed
    start_peer; connect_and_swap; arm_pressure
    OLD_PEER=$PEER_PID
    [ "$(swap_used_kb)" -gt 0 ] || abort_scenario tier_not_occupied
    if [ "$SCENARIO" = s17 ] || [ "$SCENARIO" = s18 ]; then
      [ "$($BB awk 'NR>1{n++}END{print n+0}' /proc/swaps)" = 1 ] || abort_scenario unexpected_initial_swap
      first=$(/pagetrack snapshot "$HOG" /tmp/tracked-pages) || abort_scenario initial_pagemap_failed
      log "${SCENARIO}_initial_pagemap=$first"
      case "$first" in *" other=0") ;; *) abort_scenario unexpected_initial_swap_type ;; esac
    fi
    emergency_mb=512
    [ "$SCENARIO" != s18 ] || emergency_mb=256
    if [ "$SCENARIO" = s18 ]; then
      PATH=/usr/sbin:/usr/bin:/bin /emergency-test isolated_kernel_emergency_lifecycle --ignored --nocapture >/tmp/emergency-test.log 2>&1 & EMERGENCY_PID=$!
      i=0
      while [ ! -f /tmp/emergency-ready ] && [ "$i" -lt 90 ]; do
        $BB kill -0 "$EMERGENCY_PID" 2>/dev/null || { $BB cat /tmp/emergency-test.log; abort_scenario emergency_creator_failed; }
        $BB sleep 1; i=$((i+1))
      done
      [ -f /tmp/emergency-ready ] || abort_scenario emergency_creator_timeout
      emergency_path=$($BB cat /tmp/emergency-ready)
      priority=$($BB awk -v path="$emergency_path" '$1==path{print $5}' /proc/swaps)
      [ "$priority" = 0 ] || abort_scenario emergency_priority_mismatch
      log "gate_s18_product_creator=PASS path=$emergency_path priority=$priority"
    else
    $BB dd if=/dev/zero of=/mnt/emergency/temporary.swap bs=1048576 count="$emergency_mb" conv=fsync >/tmp/emergency-create.log 2>&1 || abort_scenario emergency_create_failed
    $BB chmod 600 /mnt/emergency/temporary.swap || abort_scenario emergency_mode_failed
    $BB mkswap /mnt/emergency/temporary.swap >/dev/null 2>&1 || abort_scenario emergency_format_failed
    /swapon_prio /mnt/emergency/temporary.swap 200 >/tmp/emergency-swapon.log 2>&1 || abort_scenario emergency_activation_failed
    $BB grep -q '^/mnt/emergency/temporary[.]swap[[:space:]]' /proc/swaps || abort_scenario emergency_not_active
    priority=$($BB awk '$1=="/mnt/emergency/temporary.swap"{print $5}' /proc/swaps)
    [ "$priority" = 200 ] || abort_scenario emergency_priority_mismatch
    log "gate_${SCENARIO}_priority=PASS value=$priority"
    emergency_path=/mnt/emergency/temporary.swap
    fi
    echo 150000000 > /sys/fs/cgroup/hog/memory.max || abort_scenario emergency_pressure_failed
    i=0
    while [ "$i" -lt 45 ]; do
      used=$($BB awk -v path="$emergency_path" '$1==path{print $4}' /proc/swaps)
      [ "${used:-0}" -gt 0 ] && break
      $BB sleep 1; i=$((i+1))
    done
    [ "${used:-0}" -gt 0 ] || abort_scenario emergency_not_used
    log "${SCENARIO}_emergency_used_kb=$used"
    tier_used=$(swap_used_kb)
    [ "${tier_used:-0}" -gt 0 ] || abort_scenario tier_empty_before_drain
    log "${SCENARIO}_tier_used_before_drain_kb=$tier_used"
    drain_mode=release
    if [ "$SCENARIO" = s17 ] || [ "$SCENARIO" = s18 ]; then
      tracked=$(/pagetrack snapshot "$HOG" /tmp/tracked-pages) || abort_scenario boundary_pagemap_failed
      log "${SCENARIO}_boundary_pagemap=$tracked"
      drain_mode=hold
    fi
    report_swapoff "${SCENARIO}_tier_swapoff" 90 "$drain_mode" || abort_scenario tier_swapoff_failed
    [ "$($BB grep -c nbd0 /proc/swaps)" = 0 ] || abort_scenario tier_still_armed
    $BB awk -v path="$emergency_path" '$1==path{found=1} END{exit !found}' /proc/swaps || abort_scenario emergency_lost_during_drain
    log "${SCENARIO}_emergency_after_tier_kb=$($BB awk -v path="$emergency_path" '$1==path{print $4}' /proc/swaps)"
    if [ "$SCENARIO" = s17 ] || [ "$SCENARIO" = s18 ]; then
      [ "$($BB awk 'NR>1{n++}END{print n+0}' /proc/swaps)" = 1 ] || abort_scenario unexpected_remaining_swap
      migrated=$(/pagetrack migrated "$HOG" /tmp/tracked-pages) || abort_scenario no_tracked_page_migrated
      log "gate_${SCENARIO}_page_migration=PASS $migrated"
      echo max > /sys/fs/cgroup/hog/memory.max || abort_scenario pressure_release_failed
    fi
    /nbd-client -d /dev/nbd0 >/tmp/detach.log 2>&1 || abort_scenario detach_failed
    i=0
    while [ -n "$($BB cat /sys/block/nbd0/pid 2>/dev/null)" ] && [ "$i" -lt 10 ]; do $BB sleep 1; i=$((i+1)); done
    [ -z "$($BB cat /sys/block/nbd0/pid 2>/dev/null)" ] || abort_scenario detach_unproven
    $BB kill -TERM "$OLD_PEER" 2>/dev/null || true
    i=0
    while $BB kill -0 "$OLD_PEER" 2>/dev/null && [ "$i" -lt 10 ]; do $BB sleep 1; i=$((i+1)); done
    $BB kill -0 "$OLD_PEER" 2>/dev/null && abort_scenario old_peer_still_alive
    start_peer
    [ "$PEER_PID" != "$OLD_PEER" ] || abort_scenario peer_identity_not_changed
    connect_and_swap
    report_direct_read "${SCENARIO}_read_after_reconnect" 15 || abort_scenario reconnect_read_failed
    if [ "$SCENARIO" = s18 ]; then
      $BB touch /tmp/emergency-release
      i=0
      while [ ! -f /tmp/emergency-removed ] && [ "$i" -lt 90 ]; do
        $BB kill -0 "$EMERGENCY_PID" 2>/dev/null || { $BB cat /tmp/emergency-test.log; abort_scenario emergency_remove_failed; }
        $BB sleep 1; i=$((i+1))
      done
      [ -f /tmp/emergency-removed ] || abort_scenario emergency_remove_timeout
      wait "$EMERGENCY_PID" || abort_scenario emergency_test_failed
      log "gate_s18_product_cleanup=PASS"
    else
    bounded 90 $BB swapoff /mnt/emergency/temporary.swap || abort_scenario emergency_swapoff_failed
    $BB grep -q '^/mnt/emergency/temporary[.]swap[[:space:]]' /proc/swaps && abort_scenario emergency_still_active
    $BB rm /mnt/emergency/temporary.swap || abort_scenario emergency_remove_failed
    fi
    verify_hog "${SCENARIO}_integrity" || abort_scenario emergency_data_integrity_failed
    log "gate_${SCENARIO}_emergency_lifecycle=PASS"
    ;;
  s13|s14|s15)
    /dualpeer >/tmp/dual.log 2>&1 & DUAL=$!
    i=0
    while ! $BB grep -q '^DUAL_READY$' /tmp/dual.log; do
      $BB kill -0 "$DUAL" 2>/dev/null || { $BB cat /tmp/dual.log; abort_scenario dual_setup_failed; }
      [ "$i" -lt 30 ] || abort_scenario dual_setup_timeout
      $BB sleep 1; i=$((i+1))
    done
    PEER0=$($BB sed -n 's/^DUAL_PEER_0=//p' /tmp/dual.log)
    PEER1=$($BB sed -n 's/^DUAL_PEER_1=//p' /tmp/dual.log)
    [ -n "$PEER0" ] && [ -n "$PEER1" ] && [ "$PEER0" != "$PEER1" ] || abort_scenario dual_identity_missing
    $BB kill -0 "$PEER0" && $BB kill -0 "$PEER1" || abort_scenario dual_peer_not_alive
    $BB mkswap /dev/nbd0 >/dev/null 2>&1 || abort_scenario dual_mkswap_failed
    $BB swapon /dev/nbd0 2>/tmp/swapon.log || abort_scenario dual_swapon_failed
    arm_pressure
    used_before_kill=$(swap_used_kb)
    log "${SCENARIO}_used_before_kill_kb=$used_before_kill"
    [ "${used_before_kill:-0}" -gt 0 ] || abort_scenario dual_swap_unoccupied
    stats=$(/dualpeer --stats) || abort_scenario dual_stats_failed
    p0_writes=$(echo "$stats" | $BB sed -n 's/^P0_WRITES=//p')
    p1_writes=$(echo "$stats" | $BB sed -n 's/^P1_WRITES=//p')
    p1_reads_before=$(echo "$stats" | $BB sed -n 's/^P1_READS=//p')
    log "${SCENARIO}_pre_p0_writes=$p0_writes p1_writes=$p1_writes p1_reads=$p1_reads_before"
    [ "${p0_writes:-0}" -gt 0 ] && [ "${p1_writes:-0}" -gt 0 ] || abort_scenario dual_unexercised_peer
    if [ "$SCENARIO" = s14 ] || [ "$SCENARIO" = s15 ]; then
      if [ "$SCENARIO" = s15 ]; then
        /dualpeer --arm-partial-write || abort_scenario dual_arm_failed
      else
        /dualpeer --arm-write || abort_scenario dual_arm_failed
      fi
      operation=write
      $BB kill -ALRM "$HOG" || abort_scenario dual_dirty_signal_failed
    else
      /dualpeer --arm || abort_scenario dual_arm_failed
      operation=read
      $BB taskset -c 0 /directread /dev/nbd0 >/tmp/dual-read0.log 2>&1 & READ0=$!
      $BB taskset -c 1 /directread /dev/nbd0 >/tmp/dual-read1.log 2>&1 & READ1=$!
    fi
    i=0
    while [ "$(/dualpeer --stats | $BB sed -n 's/^P0_PAUSED=//p')" != 1 ]; do
      $BB kill -0 "$PEER0" 2>/dev/null || abort_scenario dual_peer_exited_before_pause
      [ "$i" -lt 30 ] || abort_scenario dual_inflight_not_proven
      $BB sleep 1; i=$((i+1))
    done
    log "${SCENARIO}_peer0_${operation}_inflight=YES"
    if [ "$SCENARIO" = s14 ] || [ "$SCENARIO" = s15 ]; then
      stats=$(/dualpeer --stats) || abort_scenario dual_stats_failed
      p1_writes=$(echo "$stats" | $BB sed -n 's/^P1_WRITES=//p')
      log "${SCENARIO}_p1_writes_before_kill=$p1_writes"
      [ "$SCENARIO" != s15 ] || log "s15_partial_write_applied=YES"
    fi
    $BB kill -9 "$PEER0" || abort_scenario dual_kill_failed
    i=0
    while $BB kill -0 "$PEER0" 2>/dev/null && [ "$i" -lt 10 ]; do $BB sleep 1; i=$((i+1)); done
    $BB kill -0 "$PEER0" 2>/dev/null && abort_scenario dual_death_unconfirmed
    $BB kill -0 "$PEER1" 2>/dev/null || abort_scenario dual_survivor_dead
    echo max > /sys/fs/cgroup/hog/memory.max 2>/dev/null || abort_scenario dual_cgroup_release_failed
    if [ "$SCENARIO" = s15 ]; then
      i=0
      while [ "$(/dualpeer --stats | $BB sed -n 's/^P1_REPLAY_VERIFIED=//p')" != 1 ]; do
        $BB kill -0 "$PEER1" 2>/dev/null || abort_scenario dual_survivor_dead
        [ "$i" -lt 60 ] || abort_scenario dual_exact_replay_unproven
        $BB sleep 1; i=$((i+1))
      done
      log "gate_dual_exact_replay=PASS"
    fi
    if [ "$SCENARIO" = s14 ] || [ "$SCENARIO" = s15 ]; then
      i=0
      while ! $BB grep -q '^dirty_pass bytes=419430400$' /tmp/hog.log; do
        $BB kill -0 "$HOG" 2>/dev/null || abort_scenario dual_dirty_producer_exited
        [ "$i" -lt 90 ] || abort_scenario dual_dirty_pass_stuck
        $BB sleep 1; i=$((i+1))
      done
      log "gate_dual_dirty_pass=PASS"
    fi
    verify_hog dual_peer_integrity || abort_scenario dual_integrity_failed
    stats=$(/dualpeer --stats) || abort_scenario dual_stats_after_failed
    p1_reads_after=$(echo "$stats" | $BB sed -n 's/^P1_READS=//p')
    log "${SCENARIO}_post_p1_reads=$p1_reads_after"
    [ "${p1_reads_after:-0}" -gt "${p1_reads_before:-0}" ] || abort_scenario dual_survivor_did_not_read
    if [ "$SCENARIO" = s13 ]; then
      for reader in "$READ0" "$READ1"; do
        i=0
        while $BB kill -0 "$reader" 2>/dev/null && [ "$i" -lt 30 ]; do $BB sleep 1; i=$((i+1)); done
        $BB kill -0 "$reader" 2>/dev/null && abort_scenario dual_reader_stuck
        wait "$reader" || abort_scenario dual_reader_failed
      done
    fi
    if [ "$SCENARIO" = s14 ] || [ "$SCENARIO" = s15 ]; then
      stats=$(/dualpeer --stats) || abort_scenario dual_stats_after_failed
      p1_writes_after=$(echo "$stats" | $BB sed -n 's/^P1_WRITES=//p')
      log "${SCENARIO}_post_p1_writes=$p1_writes_after"
      [ "${p1_writes_after:-0}" -gt "${p1_writes:-0}" ] || abort_scenario dual_survivor_did_not_write
    fi
    log "gate_dual_inflight_retry=PASS"
    $BB kill -0 "$PEER1" 2>/dev/null || abort_scenario dual_survivor_lost
    report_swapoff "${SCENARIO}_swapoff" 90 || abort_scenario dual_swapoff_failed
    [ "$($BB grep -c nbd0 /proc/swaps)" = 0 ] || abort_scenario dual_swap_still_armed
    /nbd-client -d /dev/nbd0 >/dev/null 2>&1 || abort_scenario dual_disconnect_failed
    log "gate_dual_peer_survival=PASS"
    ;;
  s9|s10|s11)
    # The writeback scenarios run their own script (the origin chain, the
    # zram topology and the pump are theirs).
    exec $BB sh /$SCENARIO.sh
    ;;
  *)
    log "unknown scenario '$SCENARIO'"
    ;;
esac
echo "KTEST scenario_done=$SCENARIO"
echo "=====KTEST-END====="
$BB poweroff -f
INIT
chmod +x "$IRD/init"

if [[ " $SCENARIOS " == *" s19 "* || " $SCENARIOS " == *" s20 "* ]]; then
  # Share the same pressure and integrity helpers used by the other guest
  # scenarios; generated from this immutable harness before QEMU boots.
  sed -n '/^log() { echo "KTEST \$\*"; }/,/^echo "===== scenario \$SCENARIO ====="/{ /^echo "===== scenario \$SCENARIO ====="/d; p; }' "$IRD/init" > "$IRD/dual_common.sh"
  [ -s "$IRD/dual_common.sh" ] || { echo "missing guest helpers" >&2; exit 2; }
  cat > "$IRD/s19.sh" <<'S19'
#!/bin/busybox sh
BB=/bin/busybox
SCENARIO=$($BB sed -n 's/.*ktest.scenario=\([a-z0-9]*\).*/\1/p' /proc/cmdline)
. /origin_env.sh
. /dual_common.sh
EXPECTED=$($BB cat /expected-ramsharedd.sha256)
ACTUAL=$($BB sha256sum /ramsharedd | $BB cut -d' ' -f1)
[ "${#EXPECTED}" = 64 ] && [ "$EXPECTED" = "$ACTUAL" ] || abort_scenario daemon_binary_mismatch
log "gate_${SCENARIO}_binary_match=PASS sha256=$ACTUAL"
[ ! -e /dev/dxg ] && [ ! -e /dev/nvidia0 ] || abort_scenario unexpected_gpu
$BB mkdir -p /run/ramshared
# The scenarios reuse a disposable root image. Remove only their fixture
# sockets so a previous guest cannot satisfy the listener readiness check.
$BB rm -f /run/product-peer0.sock /run/product-peer1.sock /run/product-peer2.sock
for index in 0 1; do
  $BB unshare -m --propagation private /bin/busybox sh -c '
    /bin/busybox mount -t tmpfs tmpfs /run/ramshared || exit 1
    exec /ramsharedd --origin-manifest /etc/ramshared/origin.conf --accept-wsl2-nbd --size 1024 --sock "$1" --nbd /dev/nbd0 --serve-threads 2
  ' sh "/run/product-peer${index}.sock" >"/tmp/product-peer${index}.log" 2>&1 &
  if [ "$index" = 0 ]; then PEER0=$!; else PEER1=$!; fi
done
i=0
while [ ! -S /run/product-peer0.sock ] || [ ! -S /run/product-peer1.sock ]; do
  $BB kill -0 "$PEER0" 2>/dev/null && $BB kill -0 "$PEER1" 2>/dev/null || {
    $BB cat /tmp/product-peer0.log /tmp/product-peer1.log
    abort_scenario product_peer_start_failed
  }
  [ "$i" -lt 40 ] || abort_scenario product_peer_start_timeout
  $BB sleep 1; i=$((i+1))
done
[ "$PEER0" != "$PEER1" ] || abort_scenario product_peer_identity_conflict
[ "$($BB readlink /proc/$PEER0/exe)" = /ramsharedd ] && [ "$($BB readlink /proc/$PEER1/exe)" = /ramsharedd ] || abort_scenario product_peer_executable_mismatch
NS0=$($BB stat -Lc %i /proc/$PEER0/ns/mnt); NS1=$($BB stat -Lc %i /proc/$PEER1/ns/mnt)
[ -n "$NS0" ] && [ -n "$NS1" ] && [ "$NS0" != "$NS1" ] || abort_scenario product_peer_namespaces_not_isolated
for pid in "$PEER0" "$PEER1"; do
  found=0
  for fd in /proc/$pid/fd/*; do [ "$($BB readlink "$fd" 2>/dev/null)" != /dev/vdb1 ] || found=1; done
  [ "$found" = 1 ] || abort_scenario product_origin_descriptor_missing
  i=0
  while [ ! -r "/proc/$pid/root/run/ramshared/cache-status.json" ] && [ "$i" -lt 20 ]; do
    $BB kill -0 "$pid" 2>/dev/null || abort_scenario product_peer_died_before_status
    $BB sleep 1; i=$((i+1))
  done
  $BB grep -Eq '"origin_state"[[:space:]]*:[[:space:]]*"READY"' "/proc/$pid/root/run/ramshared/cache-status.json" || abort_scenario product_origin_not_ready
  $BB grep -Eq '"cache_state"[[:space:]]*:[[:space:]]*"(OFF|UNAVAILABLE)"' "/proc/$pid/root/run/ramshared/cache-status.json" || abort_scenario product_cache_not_disabled
done
log "gate_${SCENARIO}_independent_peers=PASS"
log "gate_${SCENARIO}_origin_only=PASS"
$BB dd if=/dev/vdb1 of=/tmp/s19-header-before bs=4096 count=1 2>/dev/null || abort_scenario product_header_before_failed
HEADER_BEFORE=$($BB sha256sum /tmp/s19-header-before | $BB cut -d' ' -f1)
if [ "$SCENARIO" = s20 ]; then
  /product-dual-attach --netlink >/tmp/product-dual.log 2>&1 & ATTACH=$!
else
  /product-dual-attach >/tmp/product-dual.log 2>&1 & ATTACH=$!
fi
i=0
while ! $BB grep -q '^PRODUCT_DUAL_READY$' /tmp/product-dual.log; do
  $BB kill -0 "$ATTACH" 2>/dev/null || { $BB cat /tmp/product-dual.log; abort_scenario product_dual_attach_failed; }
  [ "$i" -lt 40 ] || abort_scenario product_dual_attach_timeout
  $BB sleep 1; i=$((i+1))
done
# The disposable origin is already signed with the exact swap UUID. Do not
# format through NBD: that would change the authoritative origin identity.
$BB swapon /dev/nbd0 2>/tmp/swapon.log || { $BB cat /tmp/swapon.log; abort_scenario product_swapon_failed; }
arm_pressure
used=$(swap_used_kb)
[ "${used:-0}" -gt 0 ] || abort_scenario product_swap_unoccupied
log "${SCENARIO}_used_before_kill_kb=$used"
$BB kill -STOP "$PEER0" || abort_scenario product_peer_stop_failed
$BB taskset -c 0 /directread /dev/nbd0 >/tmp/product-read0.log 2>&1 & READ0=$!
$BB taskset -c 1 /directread /dev/nbd0 >/tmp/product-read1.log 2>&1 & READ1=$!
$BB sleep 3
pending=0
$BB kill -0 "$READ0" 2>/dev/null && pending=$((pending+1))
$BB kill -0 "$READ1" 2>/dev/null && pending=$((pending+1))
[ "$pending" -gt 0 ] || abort_scenario no_pending_reader_observed
log "gate_${SCENARIO}_reader_pending=PASS count=$pending"
$BB kill -9 "$PEER0" || abort_scenario product_peer_kill_failed
wait "$PEER0" 2>/dev/null || true
$BB kill -0 "$PEER1" 2>/dev/null || abort_scenario product_survivor_dead
echo max > /sys/fs/cgroup/hog/memory.max || abort_scenario product_pressure_release_failed
for reader in "$READ0" "$READ1"; do
  i=0
  while $BB kill -0 "$reader" 2>/dev/null && [ "$i" -lt 90 ]; do $BB sleep 1; i=$((i+1)); done
  $BB kill -0 "$reader" 2>/dev/null && abort_scenario product_reader_stuck
  wait "$reader" || abort_scenario product_reader_failed
done
log "gate_${SCENARIO}_readers=PASS"
verify_hog "${SCENARIO}_integrity" || abort_scenario product_integrity_failed
$BB kill -0 "$PEER1" 2>/dev/null || abort_scenario product_survivor_lost
if [ "$SCENARIO" = s20 ]; then
  $BB dd if=/dev/vdb1 of=/tmp/s20-header-before-replace bs=4096 count=1 2>/dev/null || abort_scenario product_header_replace_check_failed
  [ "$HEADER_BEFORE" = "$($BB sha256sum /tmp/s20-header-before-replace | $BB cut -d' ' -f1)" ] || abort_scenario product_header_changed_before_replacement
  $BB unshare -m --propagation private /bin/busybox sh -c '
    /bin/busybox mount -t tmpfs tmpfs /run/ramshared || exit 1
    exec /ramsharedd --origin-manifest /etc/ramshared/origin.conf --accept-wsl2-nbd --size 1024 --sock /run/product-peer2.sock --nbd /dev/nbd0 --serve-threads 2
  ' >/tmp/product-peer2.log 2>&1 & PEER2=$!
  i=0
  while [ ! -S /run/product-peer2.sock ]; do
    $BB kill -0 "$PEER2" 2>/dev/null || { $BB cat /tmp/product-peer2.log; abort_scenario replacement_peer_start_failed; }
    [ "$i" -lt 40 ] || abort_scenario replacement_peer_start_timeout
    $BB sleep 1; i=$((i+1))
  done
  [ "$($BB readlink /proc/$PEER2/exe)" = /ramsharedd ] || abort_scenario replacement_executable_mismatch
  NS2=$($BB stat -Lc %i /proc/$PEER2/ns/mnt)
  [ -n "$NS2" ] && [ "$NS2" != "$NS1" ] || abort_scenario replacement_namespace_not_isolated
  found=0
  for fd in /proc/$PEER2/fd/*; do [ "$($BB readlink "$fd" 2>/dev/null)" != /dev/vdb1 ] || found=1; done
  [ "$found" = 1 ] || abort_scenario replacement_origin_descriptor_missing
  i=0
  while [ ! -r "/proc/$PEER2/root/run/ramshared/cache-status.json" ] && [ "$i" -lt 20 ]; do $BB sleep 1; i=$((i+1)); done
  $BB grep -Eq '"origin_state"[[:space:]]*:[[:space:]]*"READY"' "/proc/$PEER2/root/run/ramshared/cache-status.json" || abort_scenario replacement_origin_not_ready
  $BB grep -Eq '"cache_state"[[:space:]]*:[[:space:]]*"(OFF|UNAVAILABLE)"' "/proc/$PEER2/root/run/ramshared/cache-status.json" || abort_scenario replacement_cache_not_disabled
  before=$($BB dmesg | $BB grep -c 'reconnected socket')
  /product-dual-attach --replace-wrong >/tmp/product-wrong-backend.log 2>&1 || { $BB cat /tmp/product-wrong-backend.log; abort_scenario replacement_wrong_backend_accepted; }
  [ "$($BB dmesg | $BB grep -c 'reconnected socket')" = "$before" ] || abort_scenario replacement_wrong_backend_reconnected
  log "gate_s20_wrong_backend_refused=PASS"
  /product-dual-attach --replace >/tmp/product-replace.log 2>&1 || { $BB cat /tmp/product-replace.log; abort_scenario replacement_netlink_failed; }
  after=$($BB dmesg | $BB grep -c 'reconnected socket')
  [ "$after" -gt "$before" ] || abort_scenario replacement_kernel_confirmation_missing
  log "gate_s20_live_reattach=PASS"
  echo 200000000 > /sys/fs/cgroup/hog/memory.max || abort_scenario replacement_pressure_failed
  $BB kill -ALRM "$HOG" || abort_scenario replacement_dirty_signal_failed
  i=0
  while ! $BB grep -q '^dirty_pass bytes=419430400$' /tmp/hog.log; do
    $BB kill -0 "$HOG" 2>/dev/null || abort_scenario replacement_hog_died
    [ "$i" -lt 90 ] || abort_scenario replacement_dirty_timeout
    $BB sleep 1; i=$((i+1))
  done
  used=$(swap_used_kb)
  [ "${used:-0}" -gt 0 ] || abort_scenario replacement_swap_unoccupied
  log "s20_used_before_second_kill_kb=$used"
  cached_before=$($BB awk '/^SwapCached:/{print $2}' /proc/meminfo)
  echo 3 > /proc/sys/vm/drop_caches || abort_scenario replacement_drop_caches_failed
  cached_after=$($BB awk '/^SwapCached:/{print $2}' /proc/meminfo)
  log "s20_swap_cached_before_second_kill_kb=$cached_before"
  log "s20_swap_cached_after_second_kill_kb=$cached_after"
  tracked=$(/pagetrack snapshot "$HOG" /tmp/s20-replacement-pages) || abort_scenario replacement_pagemap_failed
  log "s20_replacement_pagemap=$tracked"
  $BB kill -9 "$PEER1" || abort_scenario second_peer_kill_failed
  wait "$PEER1" 2>/dev/null || true
  $BB kill -0 "$PEER2" 2>/dev/null || abort_scenario replacement_peer_died
  echo max > /sys/fs/cgroup/hog/memory.max || abort_scenario replacement_pressure_release_failed
  report_direct_read s20_replacement_read 90 || abort_scenario replacement_read_failed
  sectors_before=$($BB awk '{print $3}' /sys/block/nbd0/stat)
  case "$sectors_before" in ''|*[!0-9]*) abort_scenario replacement_stat_before_failed;; esac
  verify_hog s20_replacement_integrity || abort_scenario replacement_integrity_failed
  sectors_after=$($BB awk '{print $3}' /sys/block/nbd0/stat)
  case "$sectors_after" in ''|*[!0-9]*) abort_scenario replacement_stat_after_failed;; esac
  [ "$sectors_after" -gt "$sectors_before" ] || abort_scenario replacement_verification_no_device_reads
  log "s20_verification_read_sectors=$((sectors_after-sectors_before))"
  log "gate_s20_verification_device_reads=PASS"
  $BB kill -0 "$PEER2" 2>/dev/null || abort_scenario replacement_survivor_lost
  log "gate_s20_replacement_survives=PASS"
fi
report_swapoff "${SCENARIO}_swapoff" 90 || abort_scenario product_swapoff_failed
[ "$($BB grep -c nbd0 /proc/swaps)" = 0 ] || abort_scenario product_swap_still_armed
$BB dd if=/dev/vdb1 of=/tmp/s19-header-after bs=4096 count=1 2>/dev/null || abort_scenario product_header_after_failed
HEADER_AFTER=$($BB sha256sum /tmp/s19-header-after | $BB cut -d' ' -f1)
[ "$HEADER_BEFORE" = "$HEADER_AFTER" ] || abort_scenario product_origin_header_changed
log "gate_${SCENARIO}_origin_header=PASS"
/nbd-client -d /dev/nbd0 >/dev/null 2>&1 || abort_scenario product_disconnect_failed
log "gate_${SCENARIO}_survivor=PASS"
log "scenario_done=$SCENARIO"
echo "=====KTEST-END====="
$BB poweroff -f
S19
  cp "$IRD/s19.sh" "$IRD/s20.sh"
fi

( cd "$IRD" && find . | cpio -o -H newc 2>/dev/null | gzip -9 ) > "$WORK/initramfs.gz"

# s6's three virtio disks: a partitioned ext4 root (the daemon refuses an
# origin that aliases the root, so the origin needs its own disk), the
# origin partition (sealed identity: GPT PARTUUID + swap UUID) and the
# fallback partition. No root needed: mkfs -E offset and swapsig.
case " $SCENARIOS " in *" s6 "*|*" s7 "*|*" s9 "*|*" s10 "*|*" s11 "*|*" s12 "*|*" s19 "*|*" s20 "*)
  mkdir -p "$WORK/s6"
  truncate -s 256M "$WORK/s6/root.img"
  sgdisk -n 1:2048:0 -t 1:8300 "$WORK/s6/root.img" >/dev/null
  mkfs.ext4 -q -b 4096 -E offset=1048576 -F "$WORK/s6/root.img" 64000
  truncate -s 1536M "$WORK/s6/origin.img"
  sgdisk -n 1:2048:0 -t 1:8200 -u 1:11111111-2222-4333-8444-555555555555 "$WORK/s6/origin.img" >/dev/null
  "$WORK/swapsig" "$WORK/s6/origin.img" 1048576 aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee 1024
  truncate -s 512M "$WORK/s6/fallback.img"
  sgdisk -n 1:2048:0 -t 1:8200 "$WORK/s6/fallback.img" >/dev/null
  # The signed area must not exceed the partition (the GPT backup reserves
  # the tail): a smaller area is accepted and leaves the tail unused.
  "$WORK/swapsig" "$WORK/s6/fallback.img" 1048576 99999999-8888-4777-8666-555555555555 256
  ;;
esac

case " $SCENARIOS " in *" s16 "*|*" s17 "*|*" s18 "*)
  [ -f "$IRD/modules/virtio_blk.ko" ] || { echo "missing virtio block module" >&2; exit 2; }
  emergency_image_size=768M
  truncate -s "$emergency_image_size" "$WORK/s16-emergency.img"
  mkfs.ext4 -q -F "$WORK/s16-emergency.img"
  ;;
esac

OUT=${KTEST_OUT_DIR:-/tmp/opencode/nbd-campaign}
mkdir -p "$OUT"
campaign_failed=0
for SCENARIO in $SCENARIOS; do
  echo "########## boot: $SCENARIO ##########"
  ACCEL=(); SMP=2; DRIVES=()
  [ -w /dev/kvm ] && ACCEL=(-enable-kvm -cpu host)
  if [ "$SCENARIO" = s16 ] || [ "$SCENARIO" = s17 ] || [ "$SCENARIO" = s18 ]; then
    DRIVES=(-drive file="$WORK/s16-emergency.img",format=raw,if=virtio,cache=none)
  fi
  if [ "${#ACCEL[@]}" -gt 0 ]; then
    echo "KTEST host_acceleration=kvm"
  else
    echo "KTEST host_acceleration=tcg"
  fi
  if [ "$SCENARIO" = "s6" ] || [ "$SCENARIO" = "s7" ] || [ "$SCENARIO" = "s9" ] || [ "$SCENARIO" = "s10" ] || [ "$SCENARIO" = "s11" ] || [ "$SCENARIO" = "s12" ] || [ "$SCENARIO" = "s19" ] || [ "$SCENARIO" = "s20" ]; then
    if [ ! -f "$WORK/s6/root.img" ] || [ ! -f "$IRD/modules/virtio_blk.ko" ]; then
      echo "KTEST ${SCENARIO}_skipped=missing_images_or_module"
      [ "$SCENARIO" != s12 ] || campaign_failed=1
      continue
    fi
    if [ "$SCENARIO" != s12 ] && [ "$SCENARIO" != s19 ] && [ "$SCENARIO" != s20 ] && [ ! -w /dev/kvm ]; then
      echo "KTEST ${SCENARIO}_skipped=no_kvm (the rate leg measures KVM, not QEMU TCG)"; continue
    fi
    SMP=4
    DRIVES=(-drive file="$WORK/s6/root.img",format=raw,if=virtio,cache=none -drive file="$WORK/s6/origin.img",format=raw,if=virtio,cache=none -drive file="$WORK/s6/fallback.img",format=raw,if=virtio,cache=none)
  fi
  GUEST_MEMORY=1024
  [ "$SCENARIO" != s18 ] || GUEST_MEMORY=3072
  BOOT_TIMEOUT=480
  if [ "$SCENARIO" = "s7" ] || [ "$SCENARIO" = "s9" ] || [ "$SCENARIO" = "s10" ] || [ "$SCENARIO" = "s11" ] || [ "$SCENARIO" = "s12" ]; then BOOT_TIMEOUT=900; fi
  [ "$SCENARIO" != s12 ] || BOOT_TIMEOUT=240
  BACKING_ARG=""
  if [ "$SCENARIO" = "s9" ]; then BACKING_ARG=" ktest.backing=${KTEST_S9_BACKING:-/dev/nbd0}"; fi
  qemu_rc=0
  timeout --kill-after=5s "$BOOT_TIMEOUT" qemu-system-x86_64 "${ACCEL[@]}" -m "$GUEST_MEMORY" -smp "$SMP" -nographic -no-reboot \
    -kernel "$BZ" -initrd "$WORK/initramfs.gz" "${DRIVES[@]}" \
    -append "console=ttyS0 panic=1 rdinit=/init loglevel=7 cgroup_enable=memory cgroup_no_v1=all ktest.scenario=$SCENARIO$BACKING_ARG" \
    > "$OUT/serial-$SCENARIO.log" 2>&1 || qemu_rc=$?
  echo "KTEST qemu_exit=$qemu_rc"
  grep -E "^KTEST|=====KTEST" "$OUT/serial-$SCENARIO.log" || echo "KTEST no output captured"
  case "$SCENARIO" in
    s1|s2|s3|s4|s5|s12|s13|s14|s15|s16|s17|s18|s19|s20)
      if [ "$qemu_rc" -ne 0 ] || ! grep -Eq "^KTEST scenario_done=$SCENARIO[[:space:]]*$" "$OUT/serial-$SCENARIO.log" ||
         grep -q '^KTEST scenario_failed=' "$OUT/serial-$SCENARIO.log" ||
         grep -q '^KTEST gate_.*=FAIL' "$OUT/serial-$SCENARIO.log"; then
        campaign_failed=1
      fi
      if [ "$SCENARIO" = s2 ] && ! grep -Eq '^KTEST gate_peer_death_integrity=PASS([[:space:]]|$)' "$OUT/serial-$SCENARIO.log"; then
        # A completed fault injection is not proof of intact occupied pages.
        campaign_failed=1
      fi
      if [ "$SCENARIO" = s3 ] && ! grep -Eq '^KTEST gate_peer_stall_integrity=PASS([[:space:]]|$)' "$OUT/serial-$SCENARIO.log"; then
        campaign_failed=1
      fi
      if [ "$SCENARIO" = s5 ] && ! grep -Eq '^KTEST gate_s5_drain_integrity=PASS bytes=419430400 mismatches=0[[:space:]]*$' "$OUT/serial-$SCENARIO.log"; then
        campaign_failed=1
      fi
      if [ "$SCENARIO" = s16 ] || [ "$SCENARIO" = s17 ] || [ "$SCENARIO" = s18 ]; then
        if ! grep -Eq "^KTEST ${SCENARIO}_emergency_used_kb=[1-9][0-9]*[[:space:]]*$" "$OUT/serial-$SCENARIO.log" ||
           ! grep -Eq "^KTEST ${SCENARIO}_tier_used_before_drain_kb=[1-9][0-9]*[[:space:]]*$" "$OUT/serial-$SCENARIO.log" ||
           ! grep -Eq "^KTEST gate_${SCENARIO}_integrity=PASS bytes=419430400 mismatches=0[[:space:]]*$" "$OUT/serial-$SCENARIO.log" ||
           ! grep -Eq "^KTEST gate_${SCENARIO}_emergency_lifecycle=PASS[[:space:]]*$" "$OUT/serial-$SCENARIO.log" ||
           grep -Eq 'I/O error.*nbd0|MCE: Killing|SIGBUS|Kernel panic|BUG:|Oops:' "$OUT/serial-$SCENARIO.log"; then
          campaign_failed=1
        fi
        if [ "$SCENARIO" != s18 ] && ! grep -Eq "^KTEST gate_${SCENARIO}_priority=PASS value=200[[:space:]]*$" "$OUT/serial-$SCENARIO.log"; then campaign_failed=1; fi
        if [ "$SCENARIO" = s18 ] && { ! grep -q "^KTEST gate_s18_product_creator=PASS" "$OUT/serial-$SCENARIO.log" || ! grep -q "^KTEST gate_s18_product_cleanup=PASS" "$OUT/serial-$SCENARIO.log"; }; then campaign_failed=1; fi
        if [ "$SCENARIO" = s17 ] && ! grep -Eq '^KTEST gate_s17_page_migration=PASS tracked=[1-9][0-9]* migrated=[1-9][0-9]*[[:space:]]*$' "$OUT/serial-$SCENARIO.log"; then
          campaign_failed=1
        fi
      fi
      if [ "$SCENARIO" = s18 ] && ! grep -Eq '^KTEST gate_s18_page_migration=PASS tracked=[1-9][0-9]* migrated=[1-9][0-9]*[[:space:]]*$' "$OUT/serial-$SCENARIO.log"; then campaign_failed=1; fi
      if { [ "$SCENARIO" = s2 ] || [ "$SCENARIO" = s3 ]; } &&
         grep -Eq 'I/O error.*nbd0|MCE: Killing|SIGBUS|Kernel panic|BUG:|Oops:' "$OUT/serial-$SCENARIO.log"; then
        campaign_failed=1
      fi
      if [ "$SCENARIO" = s13 ] || [ "$SCENARIO" = s14 ] || [ "$SCENARIO" = s15 ]; then
        operation=read
        if [ "$SCENARIO" != s13 ]; then operation=write; fi
        if ! grep -Eq "^KTEST ${SCENARIO}_peer0_${operation}_inflight=YES[[:space:]]*$" "$OUT/serial-$SCENARIO.log" ||
           ! grep -Eq "^KTEST ${SCENARIO}_used_before_kill_kb=[1-9][0-9]*[[:space:]]*$" "$OUT/serial-$SCENARIO.log"; then campaign_failed=1; fi
        for gate in dual_peer_integrity dual_inflight_retry dual_peer_survival; do
          if ! grep -Eq "^KTEST gate_${gate}=PASS([[:space:]]|$)" "$OUT/serial-$SCENARIO.log"; then campaign_failed=1; fi
        done
        if [ "$SCENARIO" != s13 ] && ! grep -Eq '^KTEST gate_dual_dirty_pass=PASS[[:space:]]*$' "$OUT/serial-$SCENARIO.log"; then campaign_failed=1; fi
        if [ "$SCENARIO" = s15 ] && { ! grep -Eq '^KTEST s15_partial_write_applied=YES[[:space:]]*$' "$OUT/serial-$SCENARIO.log" || ! grep -Eq '^KTEST gate_dual_exact_replay=PASS[[:space:]]*$' "$OUT/serial-$SCENARIO.log"; }; then campaign_failed=1; fi
        if grep -Eq 'I/O error.*nbd0|MCE: Killing|SIGBUS|Kernel panic|BUG:|Oops:' "$OUT/serial-$SCENARIO.log"; then campaign_failed=1; fi
      fi
      if [ "$SCENARIO" = s19 ] || [ "$SCENARIO" = s20 ]; then
        for gate in binary_match independent_peers origin_only origin_header reader_pending readers survivor; do
          if ! grep -Eq "^KTEST gate_${SCENARIO}_${gate}=PASS([[:space:]]|$)" "$OUT/serial-$SCENARIO.log"; then campaign_failed=1; fi
        done
        if ! grep -Eq "^KTEST ${SCENARIO}_used_before_kill_kb=[1-9][0-9]*[[:space:]]*$" "$OUT/serial-$SCENARIO.log" ||
           ! grep -Eq "^KTEST gate_${SCENARIO}_integrity=PASS bytes=419430400 mismatches=0[[:space:]]*$" "$OUT/serial-$SCENARIO.log" ||
           grep -Eq 'I/O error.*nbd0|MCE: Killing|SIGBUS|Kernel panic|BUG:|Oops:' "$OUT/serial-$SCENARIO.log"; then campaign_failed=1; fi
      fi
      if [ "$SCENARIO" = s20 ]; then
        for gate in wrong_backend_refused live_reattach replacement_survives verification_device_reads; do
          if ! grep -Eq "^KTEST gate_s20_${gate}=PASS([[:space:]]|$)" "$OUT/serial-$SCENARIO.log"; then campaign_failed=1; fi
        done
        if ! grep -Eq '^KTEST s20_integrity_verification_sequence=1[[:space:]]*$' "$OUT/serial-$SCENARIO.log" ||
           ! grep -Eq '^KTEST s20_replacement_integrity_verification_sequence=2[[:space:]]*$' "$OUT/serial-$SCENARIO.log" ||
           ! grep -Eq '^KTEST s20_used_before_second_kill_kb=[1-9][0-9]*[[:space:]]*$' "$OUT/serial-$SCENARIO.log" ||
           ! grep -Eq '^KTEST s20_replacement_pagemap=tracked=[1-9][0-9]* other=[0-9]+[[:space:]]*$' "$OUT/serial-$SCENARIO.log" ||
           ! grep -Eq '^KTEST s20_verification_read_sectors=[1-9][0-9]*[[:space:]]*$' "$OUT/serial-$SCENARIO.log" ||
           ! grep -Eq '^KTEST gate_s20_replacement_integrity=PASS bytes=419430400 mismatches=0[[:space:]]*$' "$OUT/serial-$SCENARIO.log"; then campaign_failed=1; fi
      fi
      if [ "$SCENARIO" = s12 ]; then
        for gate in daemon_binary_match origin_seal_refusals origin_integrity origin_shutdown origin_fallback origin_kernel_clean; do
          if ! grep -Eq "^KTEST gate_${gate}=PASS([[:space:]]|$)" "$OUT/serial-$SCENARIO.log"; then
            campaign_failed=1
          fi
        done
        if [ -n "$RECOVERY_CLI" ]; then
          for gate in cli_binary_match released_cleanup_resume; do
            if ! grep -Eq "^KTEST gate_${gate}=PASS([[:space:]]|$)" "$OUT/serial-$SCENARIO.log"; then
              campaign_failed=1
            fi
          done
        fi
        if grep -Eq 'Kernel panic|WARNING:|BUG:|Oops:|I/O error|Unexpected reply|Out of memory|oom-kill|SIGBUS' "$OUT/serial-$SCENARIO.log"; then
          campaign_failed=1
        fi
      fi
      ;;
  esac
  echo "---------- kernel nbd/io lines ($SCENARIO) ----------"
  grep -E "nbd|I/O error|stuck request" "$OUT/serial-$SCENARIO.log" | tail -n 15 || true
done
echo "campaign serial logs: $OUT/serial-s*.log"
exit "$campaign_failed"
