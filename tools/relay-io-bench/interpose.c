// macOS-only process-wide libc I/O counters for locally built Relay binaries.
// DYLD_INSERT_LIBRARIES loads this into the target; SIGUSR1 resets the counters.
// A normal exit writes one JSON object to TOKN_IO_METRICS_PATH. These are calls
// through interposed libc symbols, not physical disk requests or bytes.

#include <dirent.h>
#include <fcntl.h>
#include <signal.h>
#include <stdarg.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/stat.h>
#include <sys/uio.h>
#include <time.h>
#include <unistd.h>

_Static_assert(ATOMIC_LLONG_LOCK_FREE == 2, "signal reset needs lock-free counters");

typedef struct {
  const void *replacement;
  const void *replacee;
} InterposePair;

#define INTERPOSE(replacement, replacee)                                              \
  __attribute__((used)) static const InterposePair interpose_##replacement             \
    __attribute__((section("__DATA,__interpose"))) = {                               \
      (const void *)(replacement), (const void *)(replacee)                          \
    }

static struct {
  atomic_ullong read_calls;
  atomic_ullong read_bytes;
  atomic_ullong pread_calls;
  atomic_ullong pread_bytes;
  atomic_ullong readv_calls;
  atomic_ullong readv_bytes;
  atomic_ullong preadv_calls;
  atomic_ullong preadv_bytes;
  atomic_ullong open_calls;
  atomic_ullong openat_calls;
  atomic_ullong stat_calls;
  atomic_ullong lstat_calls;
  atomic_ullong fstat_calls;
  atomic_ullong fstatat_calls;
  atomic_ullong opendir_calls;
  atomic_ullong readdir_calls;
} counters;
static atomic_ullong cpu_start_ns;

#define COUNT(field) atomic_fetch_add_explicit(&counters.field, 1, memory_order_relaxed)
#define BYTES(field, amount) atomic_fetch_add_explicit(&counters.field, (unsigned long long)(amount), memory_order_relaxed)
#define VALUE(field) atomic_load_explicit(&counters.field, memory_order_relaxed)
#define RESET(field) atomic_store_explicit(&counters.field, 0, memory_order_relaxed)

static ssize_t counted_read(int fd, void *buffer, size_t length) {
  ssize_t result = read(fd, buffer, length);
  if (result >= 0) {
    COUNT(read_calls);
    BYTES(read_bytes, result);
  }
  return result;
}
INTERPOSE(counted_read, read);

static ssize_t counted_pread(int fd, void *buffer, size_t length, off_t offset) {
  ssize_t result = pread(fd, buffer, length, offset);
  if (result >= 0) {
    COUNT(pread_calls);
    BYTES(pread_bytes, result);
  }
  return result;
}
INTERPOSE(counted_pread, pread);

static ssize_t counted_readv(int fd, const struct iovec *buffers, int count) {
  ssize_t result = readv(fd, buffers, count);
  if (result >= 0) {
    COUNT(readv_calls);
    BYTES(readv_bytes, result);
  }
  return result;
}
INTERPOSE(counted_readv, readv);

static ssize_t counted_preadv(int fd, const struct iovec *buffers, int count, off_t offset) {
  ssize_t result = preadv(fd, buffers, count, offset);
  if (result >= 0) {
    COUNT(preadv_calls);
    BYTES(preadv_bytes, result);
  }
  return result;
}
INTERPOSE(counted_preadv, preadv);

static int counted_open(const char *path, int flags, ...) {
  mode_t mode = 0;
  if (flags & O_CREAT) {
    va_list args;
    va_start(args, flags);
    mode = (mode_t)va_arg(args, int);
    va_end(args);
  }
  int result = open(path, flags, mode);
  COUNT(open_calls);
  return result;
}
INTERPOSE(counted_open, open);

static int counted_openat(int fd, const char *path, int flags, ...) {
  mode_t mode = 0;
  if (flags & O_CREAT) {
    va_list args;
    va_start(args, flags);
    mode = (mode_t)va_arg(args, int);
    va_end(args);
  }
  int result = openat(fd, path, flags, mode);
  COUNT(openat_calls);
  return result;
}
INTERPOSE(counted_openat, openat);

static int counted_stat(const char *path, struct stat *buffer) {
  int result = stat(path, buffer);
  COUNT(stat_calls);
  return result;
}
INTERPOSE(counted_stat, stat);

static int counted_lstat(const char *path, struct stat *buffer) {
  int result = lstat(path, buffer);
  COUNT(lstat_calls);
  return result;
}
INTERPOSE(counted_lstat, lstat);

static int counted_fstat(int fd, struct stat *buffer) {
  int result = fstat(fd, buffer);
  COUNT(fstat_calls);
  return result;
}
INTERPOSE(counted_fstat, fstat);

static int counted_fstatat(int fd, const char *path, struct stat *buffer, int flags) {
  int result = fstatat(fd, path, buffer, flags);
  COUNT(fstatat_calls);
  return result;
}
INTERPOSE(counted_fstatat, fstatat);

static DIR *counted_opendir(const char *path) {
  DIR *result = opendir(path);
  COUNT(opendir_calls);
  return result;
}
INTERPOSE(counted_opendir, opendir);

static struct dirent *counted_readdir(DIR *directory) {
  struct dirent *result = readdir(directory);
  COUNT(readdir_calls);
  return result;
}
INTERPOSE(counted_readdir, readdir);

static void reset_counters(int signal_number) {
  (void)signal_number;
  RESET(read_calls);
  RESET(read_bytes);
  RESET(pread_calls);
  RESET(pread_bytes);
  RESET(readv_calls);
  RESET(readv_bytes);
  RESET(preadv_calls);
  RESET(preadv_bytes);
  RESET(open_calls);
  RESET(openat_calls);
  RESET(stat_calls);
  RESET(lstat_calls);
  RESET(fstat_calls);
  RESET(fstatat_calls);
  RESET(opendir_calls);
  RESET(readdir_calls);

  struct timespec now;
  if (clock_gettime(CLOCK_PROCESS_CPUTIME_ID, &now) == 0) {
    unsigned long long nanoseconds = (unsigned long long)now.tv_sec * 1000000000ULL + (unsigned long long)now.tv_nsec;
    atomic_store_explicit(&cpu_start_ns, nanoseconds, memory_order_relaxed);
  }
}

static void write_metrics(void) {
  const char *path = getenv("TOKN_IO_METRICS_PATH");
  if (path == NULL || *path == '\0') {
    return;
  }

  unsigned long long cpu_total_ns = 0;
  struct timespec now;
  if (clock_gettime(CLOCK_PROCESS_CPUTIME_ID, &now) == 0) {
    unsigned long long end_ns = (unsigned long long)now.tv_sec * 1000000000ULL + (unsigned long long)now.tv_nsec;
    unsigned long long start_ns = atomic_load_explicit(&cpu_start_ns, memory_order_relaxed);
    cpu_total_ns = end_ns >= start_ns ? end_ns - start_ns : 0;
  }

  char output[1024];
  int length = snprintf(
    output, sizeof(output),
    "{\"read_calls\":%llu,\"read_bytes\":%llu,\"pread_calls\":%llu,\"pread_bytes\":%llu,"
    "\"readv_calls\":%llu,\"readv_bytes\":%llu,\"preadv_calls\":%llu,\"preadv_bytes\":%llu,"
    "\"open_calls\":%llu,\"openat_calls\":%llu,\"stat_calls\":%llu,\"lstat_calls\":%llu,"
    "\"fstat_calls\":%llu,\"fstatat_calls\":%llu,\"opendir_calls\":%llu,\"readdir_calls\":%llu,"
    "\"cpu_total_ns\":%llu}\n",
    VALUE(read_calls), VALUE(read_bytes), VALUE(pread_calls), VALUE(pread_bytes),
    VALUE(readv_calls), VALUE(readv_bytes), VALUE(preadv_calls), VALUE(preadv_bytes),
    VALUE(open_calls), VALUE(openat_calls), VALUE(stat_calls), VALUE(lstat_calls),
    VALUE(fstat_calls), VALUE(fstatat_calls), VALUE(opendir_calls), VALUE(readdir_calls), cpu_total_ns
  );
  if (length <= 0 || (size_t)length >= sizeof(output)) {
    return;
  }

  // Calls originating in this dylib resolve to libc directly; reporting does
  // not enter the interposed functions or alter the captured counts.
  int fd = open(path, O_WRONLY | O_CREAT | O_TRUNC, 0600);
  if (fd >= 0) {
    size_t written = 0;
    while (written < (size_t)length) {
      ssize_t result = write(fd, output + written, (size_t)length - written);
      if (result <= 0) {
        break;
      }
      written += (size_t)result;
    }
    (void)close(fd);
  }
}

__attribute__((constructor)) static void install_metrics(void) {
  struct sigaction action = {0};
  action.sa_handler = reset_counters;
  sigemptyset(&action.sa_mask);
  (void)sigaction(SIGUSR1, &action, NULL);
  (void)atexit(write_metrics);
}
