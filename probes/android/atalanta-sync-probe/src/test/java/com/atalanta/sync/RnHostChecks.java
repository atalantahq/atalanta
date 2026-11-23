package com.atalanta.sync;

import static com.atalanta.sync.HostChecks.check;
import static com.atalanta.sync.TrackerChecks.byName;

import com.facebook.react.ReactApplication;
import com.facebook.react.ReactHost;
import com.facebook.react.ReactNativeHost;
import com.facebook.react.defaults.DefaultReactHost;
import com.facebook.react.runtime.ReactHostImpl;
import expo.modules.ExpoReactHostFactory;
import java.util.List;
import java.util.function.Supplier;

/**
 * Bridgeless host discovery without calling getReactHost() (review-fi4
 * MAJOR-2): hosts kept somewhere other than DefaultReactHost.reactHost.
 */
final class RnHostChecks {
    private RnHostChecks() {}

    static void run() {
        expoFactory();
        lazyDelegate();
        companionStatic();
        wrapperField();
        notFound();
    }

    /** A bridgeless app whose getReactHost() is {@code make}; the probe must not call it. */
    abstract static class App implements ReactApplication {
        /** Typed Object: the probe's field search must not see the app's own copy. */
        private final Object hostObject = new ReactHostImpl();
        int calls;

        ReactHostImpl impl() {
            return (ReactHostImpl) hostObject;
        }

        @Override
        public ReactNativeHost getReactNativeHost() {
            throw new UnsupportedOperationException("bridgeless");
        }

        @Override
        public ReactHost getReactHost() {
            calls++;
            return make();
        }

        abstract ReactHost make();
    }

    /** Checks the probe finds the host once the app made it, and never makes it. */
    static void finds(App app, String what) {
        DefaultReactHost.reset();
        TrackerChecks.Clock clock = new TrackerChecks.Clock();
        ReactNativeIdling rn = new ReactNativeIdling(app, UrlBlocklist.SHARED, clock);
        List<Tracker> ts = rn.trackers();
        boolean before = byName(ts, "rn_loading", -1) == 1 && app.calls == 0;
        app.getReactHost(); // ReactActivity.onCreate
        TrackerChecks.Rn r = new TrackerChecks.Rn(false);
        app.impl().context = r.ctx;
        r.fabric.dispatcher().addMountItem("x");
        boolean after = byName(ts, "rn_loading", -1) == 0 && byName(ts, "rn_mounting", -1) == 1 && app.calls == 1;
        check(before && after, "fi4-MAJOR-2: " + what + ": found once made, never made by the probe");
    }

    static void expoFactory() {
        ExpoReactHostFactory.reset();
        App app = new App() {
            @Override
            ReactHost make() {
                return ExpoReactHostFactory.getDefaultReactHost(() -> impl());
            }
        };
        finds(app, "Expo's ExpoReactHostFactory.reactHost");
        check(ExpoReactHostFactory.created == 1, "fi4-MAJOR-2: ... made once (by the app)");
        ExpoReactHostFactory.reset();
    }

    /** {@code override val reactHost by lazy { ... }}: a {@code reactHost$delegate} field. */
    static final class LazyHost<T> implements kotlin.Lazy<T> {
        private final Supplier<T> make;
        private T value;

        LazyHost(Supplier<T> make) {
            this.make = make;
        }

        @Override
        public T getValue() {
            if (value == null) value = make.get();
            return value;
        }

        @Override
        public boolean isInitialized() {
            return value != null;
        }
    }

    static final class LazyApp extends App {
        private final kotlin.Lazy<ReactHost> reactHost$delegate = new LazyHost<>(() -> impl());

        @Override
        ReactHost make() {
            return reactHost$delegate.getValue();
        }
    }

    static void lazyDelegate() {
        LazyApp app = new LazyApp();
        finds(app, "a by-lazy reactHost$delegate (isInitialized first)");
    }

    /** {@code companion object { var host: ReactHost? }}: a static of the app class. */
    static final class CompanionApp extends App {
        private static ReactHost cachedHost;

        @Override
        ReactHost make() {
            if (cachedHost == null) cachedHost = impl();
            return cachedHost;
        }
    }

    static void companionStatic() {
        finds(new CompanionApp(), "a companion object (static) ReactHost field");
        CompanionApp.cachedHost = null;
    }

    /** A ReactNativeHost wrapper that keeps the ReactHost it made. */
    static final class Wrapper extends ReactNativeHost {
        ReactHost made;
    }

    static final class WrapperApp extends App {
        final Wrapper wrapper = new Wrapper();

        @Override
        public ReactNativeHost getReactNativeHost() {
            return wrapper;
        }

        @Override
        ReactHost make() {
            if (wrapper.made == null) wrapper.made = impl();
            return wrapper.made;
        }
    }

    static void wrapperField() {
        finds(new WrapperApp(), "a ReactHost kept by the app's ReactNativeHost (wrapper)");
    }

    /** Somewhere the probe can't see: a static of an unrelated class. */
    static final class Elsewhere {
        static ReactHost host;
    }

    static void notFound() {
        DefaultReactHost.reset();
        App app = new App() {
            @Override
            ReactHost make() {
                if (Elsewhere.host == null) Elsewhere.host = impl();
                return Elsewhere.host;
            }
        };
        TrackerChecks.Clock clock = new TrackerChecks.Clock();
        ReactNativeIdling rn = new ReactNativeIdling(app, UrlBlocklist.SHARED, clock);
        List<Tracker> ts = rn.trackers();
        boolean loading = byName(ts, "rn_loading", -1) == 1;
        app.getReactHost();
        TrackerChecks.Rn r = new TrackerChecks.Rn(false);
        app.impl().context = r.ctx;
        clock.now.addAndGet(ReactNativeIdling.HOST_FIND_MS / 2);
        boolean stillLoading = byName(ts, "rn_loading", -1) == 1;
        clock.now.addAndGet(ReactNativeIdling.HOST_FIND_MS);
        boolean allUnknown = true;
        for (Tracker t : ts) allUnknown &= t.pending(-1) == Tracker.UNKNOWN;
        check(loading && stillLoading && allUnknown && ProbeLog.warned("rn-no-host") && app.calls == 1,
                "fi4-MAJOR-2: a host the probe can't find is loading for " + (ReactNativeIdling.HOST_FIND_MS / 1000)
                        + " s, then every rn_* tracker is unknown, with one warning (not 30 s of loading, then 0)");
        Elsewhere.host = null;
    }
}
