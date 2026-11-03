package com.example.demo;

import java.util.List;

/** A greeter. */
public class Sample extends Base implements Runnable {
    private static final int MAX = 10;
    private final String name;

    public Sample(String name) {
        this.name = name;
    }

    @Override
    public void run() {
        List<String> parts = List.of("a", "b");
        for (String p : parts) {
            System.out.println(p + name);
        }
    }

    enum Level { LOW, HIGH }

    interface Visitor<T> {
        T visit(Sample s);
    }
}
