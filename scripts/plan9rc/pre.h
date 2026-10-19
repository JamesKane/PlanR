/*
 * Included before each of Plan 9's rc sources, to build them on Linux
 * with plan9port's libraries (scripts/build-plan9-rc.sh).
 */
#include <u.h>
#include <libc.h>

/* plan9port's libc.h has its own searchpath. */
#define searchpath rcsearchpath
#define RFNOMNT 0

/* Plan 9's names for the environment, rcmain and descriptors (shim.c). */
char *rc9path(char*);
#undef open
#undef create
#define open(f, m) p9open(rc9path(f), m)
#define create(f, m, p) p9create(rc9path(f), m, p)
#define remove(f) remove(rc9path(f))
#define access(f, m) access(rc9path(f), m)
#define dirstat(f) dirstat(rc9path(f))

/* Unix pipes, which /dev/fd can open; lib9's are socketpairs. */
#undef pipe
int pipe(int*);
