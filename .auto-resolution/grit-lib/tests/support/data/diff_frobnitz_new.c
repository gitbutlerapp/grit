/* Grit diff fixture: patience/histogram stress (independent of upstream Git tests). */
#include <stdio.h>

static int walk(int n)
{
    if (n > 2) {
        return walk(n - 1) + walk(n - 2);
    }
    return 1;
}

static int scale(int n)
{
    for (int i = 0; i < 10; i++) {
        printf("%d\n", n);
    }
    return 0;
}

int main(void)
{
    scale(walk(10));
    return 0;
}
