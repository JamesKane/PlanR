/*
 * What Plan 9's rc wants of Plan 9 that plan9port's lib9 lacks, on Linux
 * (scripts/build-plan9-rc.sh).
 */
#include <u.h>
#include <libc.h>
#include <unistd.h>
#include <stdlib.h>

int
fd2path(int fd, char *buf, int nbuf)
{
	char p[64];
	int n;

	snprint(p, sizeof p, "/proc/self/fd/%d", fd);
	n = readlink(p, buf, nbuf-1);
	if(n < 0)
		return -1;
	buf[n] = 0;
	return 0;
}

/*
 * Plan 9's names for the environment, rcmain and the descriptors:
 * /env and /rc/lib/rcmain are kept under $RC9ROOT (its env and rcmain),
 * #d/N and /fd/N are /dev/fd/N. Each result is good for 8 calls.
 */
char*
rc9path(char *f)
{
	static char buf[8][1024];
	static int i;
	char *root, *b;

	root = getenv("RC9ROOT");
	b = buf[i++ % 8];
	if(root && (strcmp(f, "/env") == 0 || strncmp(f, "/env/", 5) == 0))
		snprint(b, 1024, "%s%s", root, f);
	else if(root && strcmp(f, "/rc/lib/rcmain") == 0)
		snprint(b, 1024, "%s/rcmain", root);
	else if(strncmp(f, "#d/", 3) == 0)
		snprint(b, 1024, "/dev/fd/%s", f+3);
	else if(strncmp(f, "/fd/", 4) == 0)
		snprint(b, 1024, "/dev/fd/%s", f+4);
	else
		return f;
	return b;
}
