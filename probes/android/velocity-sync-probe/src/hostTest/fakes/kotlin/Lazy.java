package kotlin;

/** Host-check fake of Kotlin's {@code Lazy} (a {@code by lazy} delegate field's type). */
public interface Lazy<T> {
    T getValue();

    boolean isInitialized();
}
