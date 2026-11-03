#include <stdio.h>
#define MAX 10
#define SQUARE(x) ((x) * (x))

typedef struct Point {
    int x;
    int y;
} Point;

enum Color { RED, GREEN };

static int add(int a, int b) {
    return a + b;
}

char *name(void) {
    return "lector";
}

int main(int argc, char **argv) {
    Point p = {1, 2};
    for (int i = 0; i < MAX; i++) {
        printf("%d\n", add(p.x, i));
    }
    return 0;
}
