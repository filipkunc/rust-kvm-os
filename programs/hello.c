#include <stdio.h>

int main(void) {
    printf("hello from Linux userspace\n");
    for (int i = 1; i <= 3; i++) {
        printf("  %d squared is %d\n", i, i * i);
    }
    return 42;
}
