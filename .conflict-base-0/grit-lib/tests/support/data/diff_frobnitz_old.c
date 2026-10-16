/* Grit diff fixture: multi-hunk C-like layout (independent of upstream Git tests). */
#include <stdio.h>

static int scale(int n)
{
    int acc = 0;
    for (int i = 0; i < 10; i++) {
        acc += n;
    }
    return acc;
}

static int product_upto(int n)
{
    if (n > 1) {
        return product_upto(n - 1) * n;
    }
    return 1;
}

int main(void)
{
    scale(product_upto(10));
    return 0;
}
