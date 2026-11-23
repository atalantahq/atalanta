package android.util;

import java.util.TreeMap;

/** Host-check fake of SparseArray (android.jar's stub throws): size(), valueAt(i), put, remove. */
public class SparseArray<E> {
    /**
     * The UI thread, when a check sets it: size()/valueAt() compact the real
     * SparseArray (gc()), so reads from any other thread are counted.
     */
    public static volatile Thread owner;
    public static final java.util.concurrent.atomic.AtomicInteger offThreadReads = new java.util.concurrent.atomic.AtomicInteger();

    private final TreeMap<Integer, E> map = new TreeMap<>();

    private static void read() {
        Thread o = owner;
        if (o != null && Thread.currentThread() != o) offThreadReads.incrementAndGet();
    }

    public int size() {
        read();
        return map.size();
    }

    @SuppressWarnings("unchecked")
    public E valueAt(int index) {
        read();
        return (E) map.values().toArray()[index];
    }

    public void put(int key, E value) {
        map.put(key, value);
    }

    public void remove(int key) {
        map.remove(key);
    }

    public void clear() {
        map.clear();
    }
}
