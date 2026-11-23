package expo.modules;

import com.facebook.react.ReactHost;
import java.util.function.Supplier;

/**
 * Host-check fake of Expo's {@code object ExpoReactHostFactory} as assumed by
 * the probe (unverified against Expo's source): a private static
 * {@code reactHost} filled by {@code getDefaultReactHost} on first call.
 */
public final class ExpoReactHostFactory {
    private static ReactHost reactHost;
    public static int created;

    private ExpoReactHostFactory() {}

    public static ReactHost getDefaultReactHost(Supplier<ReactHost> make) {
        if (reactHost == null) {
            created++;
            reactHost = make.get();
        }
        return reactHost;
    }

    public static void reset() {
        reactHost = null;
        created = 0;
    }
}
