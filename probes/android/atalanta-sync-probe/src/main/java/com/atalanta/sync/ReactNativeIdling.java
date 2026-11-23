// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

package com.atalanta.sync;

import java.lang.reflect.Field;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.HashMap;
import java.util.HashSet;
import java.util.List;
import java.util.Map;
import java.util.Set;

/**
 * React Native's own pending work, read by reflection from the app's
 * {@code ReactApplication} (old architecture: {@code ReactNativeHost} ->
 * {@code ReactInstanceManager}; bridgeless: {@code ReactHost}). Turned on
 * by a host connection's {@code config} with
 * {@code "framework":"react_native"} (the suite's {@code framework:
 * react_native}). The probe has no compile-time dependency on React Native.
 *
 * <ul>
 * <li>{@code rn_loading}: the app is a {@code ReactApplication} with no
 *     React context (the JS bundle is loading, or reloading). Busy for at
 *     most {@link #LOADING_MAX_MS} from the moment the context went
 *     missing, then unknown.</li>
 * <li>{@code rn_timers}: JS timers ({@code setTimeout}) shorter than the
 *     horizon, RN's own rule ({@code !repeat && interval < range}). Read
 *     from {@code JavaTimerManager.timers} under its {@code timerGuard}
 *     (both architectures; bridgeless has no {@code TimingModule}), else
 *     {@code TimingModule.hasActiveTimersInRange} (public), else
 *     {@code JavaTimerManager.hasActiveTimersInRange}, which is Kotlin
 *     {@code internal} and so compiled as
 *     {@code hasActiveTimersInRange$ReactAndroid_release}.</li>
 * <li>{@code rn_mounting}: Fabric's {@code MountItemDispatcher} queues
 *     ({@code mMountItems}, {@code mViewCommandMountItems}); on the old
 *     architecture {@code UIViewOperationQueue.isEmpty()}.</li>
 * <li>{@code rn_animation}: the active drivers of
 *     {@code NativeAnimatedNodesManager} (an animation driven with
 *     {@code useNativeDriver}); the manager is
 *     {@code NativeAnimatedModule.mNodesManager} (RN 0.77, Java) or
 *     {@code nodesManagerRef} (RN 0.81, Kotlin), both an
 *     {@code AtomicReference}.</li>
 * <li>{@code rn_network}: running and queued calls on the OkHttp client of
 *     RN's {@code NetworkingModule} ({@code fetch}/XHR), minus blocklisted
 *     URLs.</li>
 * </ul>
 *
 * <p>Perpetual work stops counting and is reported as
 * {@code ignored: perpetual <tracker>}: a JS timer re-armed as it fires
 * ({@code setTimeout} polling, a {@code requestAnimationFrame} loop; see
 * {@link RepostChains}), a looping native animation
 * ({@code Animated.loop} without {@code iterations}: {@code iterations == -1};
 * or restarted by JS again and again; a finite loop counts until it ends) running for {@link #PERPETUAL_MS},
 * and a {@code fetch} open longer than {@link #STREAM_MAX_MS} (a stream).
 * One-shot work keeps counting.
 *
 * <p>Each read is defensive: a class, field or method that isn't there
 * (another RN version) makes that tracker unknown with one warning, never
 * idle and never a crash. Reads happen on the probe's thread and only read
 * existing objects: the {@code ReactHost} is taken only once something made
 * it ({@code getReactHost()} creates one; see {@link #existingReactHost}),
 * and a native module only once it exists ({@code getNativeModule} creates
 * it). When no host (and no {@code ReactInstanceManager}) is found for
 * {@link #HOST_FIND_MS}, every RN tracker is unknown, with one warning.
 */
public final class ReactNativeIdling {
    /** How long a missing React context counts as "loading". */
    public static final long LOADING_MAX_MS = 30_000;
    /** No ReactHost / ReactInstanceManager found this long: RN tracking is unknown (not "loading"). */
    public static final long HOST_FIND_MS = 10_000;
    /** A looping animation counts for this long, then it is perpetual. */
    public static final long PERPETUAL_MS = 2_000;
    /** A call running longer than this is a stream (SSE, a long download): not waited for. */
    public static final long STREAM_MAX_MS = 10_000;
    /** Without the drivers, an animation active this long without a break is perpetual. */
    static final long ANIMATION_FALLBACK_MS = 5_000;
    /** {@code UIManagerType.FABRIC}. */
    static final int FABRIC = 2;

    static final String TIMING_MODULE = "com.facebook.react.modules.core.TimingModule";
    static final String ANIMATED_MODULE = "com.facebook.react.animated.NativeAnimatedModule";
    static final String NETWORKING_MODULE = "com.facebook.react.modules.network.NetworkingModule";
    static final String UI_MANAGER_MODULE = "com.facebook.react.uimanager.UIManagerModule";
    static final String UI_MANAGER_HELPER = "com.facebook.react.uimanager.UIManagerHelper";
    static final String REACT_APPLICATION = "com.facebook.react.ReactApplication";
    static final String REACT_HOST = "com.facebook.react.ReactHost";
    static final String DEFAULT_REACT_HOST = "com.facebook.react.defaults.DefaultReactHost";
    /**
     * Where {@code getReactHost()} implementations keep the host they made
     * ({@code object}s' private {@code reactHost}: a private static):
     * RN's template, and Expo's {@code ExpoReactHostFactory} (unverified: no
     * Expo source was available; a missing class is skipped).
     */
    static final String[][] HOST_HOLDERS = {
        {DEFAULT_REACT_HOST, "reactHost"},
        {"expo.modules.ExpoReactHostFactory", "reactHost"},
    };
    static final String KOTLIN_LAZY = "kotlin.Lazy";
    static final String REACT_MODULE = "com.facebook.react.module.annotations.ReactModule";

    public static final LooperTimers.Clock SYSTEM_CLOCK = () -> System.nanoTime() / 1_000_000;

    private final Object app;
    private final UrlBlocklist blocklist;
    private final LooperTimers.Clock clock;
    /** Posts to the UI thread (null: no Android main looper, the host JVM: read inline). */
    private final MainThreadSampler uiSampler;
    private volatile Object uiNodes;
    private final java.util.concurrent.atomic.AtomicReference<Object[]> uiResult = new java.util.concurrent.atomic.AtomicReference<>();
    private int animLast;
    private final boolean isReactApp;
    private final List<Tracker> trackers;

    /** When the React context was last seen missing (MIN_VALUE: it is there). */
    private long missingSince = Long.MIN_VALUE;
    /** Since when neither a ReactHost nor a ReactInstanceManager was found (MIN_VALUE: one was). */
    private long noHostSince = Long.MIN_VALUE;
    /** No host found for {@link #HOST_FIND_MS}: every tracker is unknown. */
    private volatile boolean hostNotFound;

    private final RepostChains timerChains = new RepostChains();
    private volatile boolean timersPerpetual;
    /** The boolean fallback: since when rn_timers has been busy without a break. */
    private long timersBusySince = Long.MIN_VALUE;
    private long timersLastSample = Long.MIN_VALUE;

    /** Driver id -> first seen (ms). */
    private final Map<Integer, Long> driverSeen = new HashMap<>();
    private long animBusySince = Long.MIN_VALUE;
    private long animLastSample = Long.MIN_VALUE;
    private Set<Integer> animLastIds = Collections.emptySet();
    private int animRearms;
    private int animLoopWidth;
    private volatile boolean animationPerpetual;

    /** Call identity -> first seen (ms). */
    private final Map<Integer, Long> callSeen = new HashMap<>();
    private volatile boolean networkPerpetual;

    public ReactNativeIdling(Object app, UrlBlocklist blocklist, LooperTimers.Clock clock) {
        this(app, blocklist, clock, mainPoster());
    }

    ReactNativeIdling(Object app, UrlBlocklist blocklist, LooperTimers.Clock clock, MainThreadCheck.Poster ui) {
        this.app = app;
        this.blocklist = blocklist;
        this.clock = clock;
        this.uiSampler = ui == null ? null : new MainThreadSampler(ui, this::readNodesOnUi, MainThreadCheck.BUDGET_MS, 0);
        this.isReactApp = isReactApplication(app);
        if (!isReactApp) {
            ProbeLog.warnOnce("rn-app", "framework react_native, but the Application is not a ReactApplication"
                    + " (or AtalantaSyncProbe.start was not given a Context): React Native work not waited for");
        }
        this.trackers = Collections.unmodifiableList(Arrays.<Tracker>asList(
                new Named("rn_loading", this::loading, () -> false),
                new Named("rn_timers", this::timers, () -> timersPerpetual),
                new Named("rn_mounting", m -> mounting(), () -> false),
                new Named("rn_animation", m -> animation(), () -> animationPerpetual),
                new Named("rn_network", m -> network(), () -> networkPerpetual)));
    }

    /** The Android main thread (inline when already on it); null without a main looper (host JVM). */
    static MainThreadCheck.Poster mainPoster() {
        try {
            final android.os.Looper main = android.os.Looper.getMainLooper();
            if (main == null) return null;
            final android.os.Handler handler = new android.os.Handler(main);
            return r -> {
                if (android.os.Looper.myLooper() == main) {
                    r.run();
                    return true;
                }
                return handler.postAtFrontOfQueue(r);
            };
        } catch (Throwable t) {
            return null; // android.jar stubs on the host JVM
        }
    }

    /** Whether {@code app} implements {@code ReactApplication}. */
    public static boolean isReactApplication(Object app) {
        if (app == null) return false;
        Class<?> c = Reflect.cls(REACT_APPLICATION, app.getClass().getClassLoader());
        return c != null && c.isInstance(app);
    }

    /** Whether this was given an Application (false: made before {@code start(context)}). */
    boolean hasApplication() {
        return app != null;
    }

    /** The trackers (the same instances every call). */
    public List<Tracker> trackers() {
        return trackers;
    }

    List<Tracker> trackersCreated() {
        return trackers;
    }

    /** A tracker from a name and a read. */
    private static final class Named implements Tracker {
        interface Read {
            int read(long maxTimerMs);
        }

        interface Flag {
            boolean on();
        }

        private final String name;
        private final Read read;
        private final Flag perpetual;

        Named(String name, Read read, Flag perpetual) {
            this.name = name;
            this.read = read;
            this.perpetual = perpetual;
        }

        @Override
        public String name() {
            return name;
        }

        @Override
        public int pending(long maxTimerMs) {
            return read.read(maxTimerMs);
        }

        @Override
        public List<String> ignoredPerpetual() {
            return perpetual.on() ? Collections.singletonList(name) : Collections.<String>emptyList();
        }
    }

    // --- the React context -------------------------------------------------

    /** The current ReactContext, null when none yet, MISSING when it can't be reached. */
    synchronized Object context() {
        if (!isReactApp) return Reflect.MISSING;
        boolean reachable = false;
        boolean found = false;
        // Bridgeless (new architecture): the ReactHost, only once it exists.
        Object host = existingReactHost();
        if (!Reflect.missing(host) && host != null) {
            reachable = true;
            found = true;
            Object ctx = Reflect.call(host, "getCurrentReactContext");
            if (!Reflect.missing(ctx) && ctx != null) return hostFound(ctx);
        } else if (host == null) {
            reachable = true; // not made yet (or kept where the probe can't see it)
        }
        Object nativeHost = Reflect.call(app, "getReactNativeHost");
        if (!Reflect.missing(nativeHost) && nativeHost != null) {
            // getReactInstanceManager() creates one when there is none.
            Object has = Reflect.call(nativeHost, "hasInstance");
            if (Boolean.TRUE.equals(has)) {
                found = true;
                Object ctx = Reflect.call(Reflect.call(nativeHost, "getReactInstanceManager"), "getCurrentReactContext");
                if (!Reflect.missing(ctx) && ctx != null) return hostFound(ctx);
                if (!Reflect.missing(ctx)) reachable = true;
            } else if (Boolean.FALSE.equals(has)) {
                reachable = true;
            }
        }
        if (!reachable) return Reflect.MISSING;
        if (found) return hostFound(null);
        // Nothing made yet: the activity hasn't started, or the host lives where the probe can't see it.
        long now = clock.uptimeMs();
        if (noHostSince == Long.MIN_VALUE) noHostSince = now;
        if (now - noHostSince > HOST_FIND_MS) {
            if (!hostNotFound) {
                hostNotFound = true;
                ProbeLog.warnOnce("rn-no-host", "no ReactHost or ReactInstanceManager found " + (HOST_FIND_MS / 1000)
                        + " s after React Native tracking started (the app's getReactHost() keeps it somewhere the"
                        + " probe can't see without creating one): rn_* trackers unknown, React Native work not waited for");
            }
            return Reflect.MISSING;
        }
        return null;
    }

    private Object hostFound(Object ctx) {
        noHostSince = Long.MIN_VALUE;
        hostNotFound = false;
        return ctx;
    }

    /**
     * The app's ReactHost without creating one. {@code getReactHost()} is
     * not called: the RN template's is
     * {@code DefaultReactHost.getDefaultReactHost(...)}, which builds the
     * host on first call, unsynchronised. Instead, in order:
     * <ol>
     * <li>the holders {@code getReactHost()} implementations fill
     *     ({@link #HOST_HOLDERS}: {@code DefaultReactHost.reactHost}, Expo's
     *     {@code ExpoReactHostFactory.reactHost});</li>
     * <li>a {@code ReactHost} field (instance or static, so a Kotlin
     *     {@code companion object} too) of the Application or of its
     *     {@code ReactNativeHost} (a wrapper may keep it), or an initialised
     *     {@code by lazy} delegate of one ({@code kotlin.Lazy.isInitialized()}
     *     first: {@code getValue()} would create it).</li>
     * </ol>
     * Only an RN without {@code DefaultReactHost} at all is asked through
     * {@code getReactHost()}. Null: none found (yet); MISSING: not a
     * bridgeless app (or unreachable).
     */
    private Object existingReactHost() {
        ClassLoader loader = app.getClass().getClassLoader();
        boolean holders = false;
        for (String[] h : HOST_HOLDERS) {
            Object cached = Reflect.staticField(Reflect.cls(h[0], loader), h[1]);
            if (Reflect.missing(cached)) continue;
            holders = true;
            if (cached != null) return cached;
        }
        if (holders) {
            Class<?> hostType = Reflect.cls(REACT_HOST, loader);
            Object own = hostIn(app, hostType, loader);
            if (own == null) {
                Object nativeHost = Reflect.call(app, "getReactNativeHost");
                if (!Reflect.missing(nativeHost) && nativeHost != null) own = hostIn(nativeHost, hostType, loader);
            }
            if (own != null) return own;
            return Reflect.missing(Reflect.call(app, "getReactNativeHost")) ? null : Reflect.MISSING;
        }
        Object host = Reflect.call(app, "getReactHost");
        return Reflect.missing(host) || host == null ? Reflect.MISSING : host;
    }

    /** A ReactHost {@code owner} keeps in a field (or an initialised lazy one), or null. */
    private static Object hostIn(Object owner, Class<?> hostType, ClassLoader loader) {
        if (hostType == null) return null;
        Class<?> lazyType = Reflect.cls(KOTLIN_LAZY, loader);
        for (Class<?> c = owner.getClass(); c != null && c != Object.class; c = c.getSuperclass()) {
            for (Field f : c.getDeclaredFields()) {
                boolean isStatic = java.lang.reflect.Modifier.isStatic(f.getModifiers());
                boolean isHost = hostType.isAssignableFrom(f.getType());
                boolean isLazy = lazyType != null && lazyType.isAssignableFrom(f.getType());
                if (!isHost && !isLazy) continue;
                try {
                    f.setAccessible(true);
                    Object v = f.get(isStatic ? null : owner);
                    if (v == null) continue;
                    if (isHost) return v;
                    // by lazy: only once something else made it.
                    if (!Boolean.TRUE.equals(Reflect.call(v, "isInitialized"))) continue;
                    Object value = Reflect.call(v, "getValue");
                    if (hostType.isInstance(value)) return value;
                } catch (Throwable t) {
                    // next
                }
            }
        }
        return null;
    }

    /**
     * The native module when it already exists; null when it doesn't
     * (never created here); MISSING when the class isn't there.
     */
    private Object module(Object ctx, String className) {
        Class<?> c = Reflect.cls(className, ctx.getClass().getClassLoader());
        if (c == null) return Reflect.MISSING;
        // Bridgeless: true only once the TurboModule exists. Old arch: registered.
        Object has = Reflect.call(ctx, "hasNativeModule", c);
        if (Boolean.FALSE.equals(has)) return null;
        if (Boolean.FALSE.equals(oldArchHasInstance(ctx, c))) return null;
        Object m = Reflect.call(ctx, "getNativeModule", c);
        return m == null ? Reflect.MISSING : m;
    }

    /**
     * Old architecture: whether the module's {@code ModuleHolder} has made
     * it ({@code BridgeReactContext.mCatalystInstance.mNativeModuleRegistry}).
     * Null when that can't be read (then {@code getNativeModule} is used).
     */
    private static Object oldArchHasInstance(Object ctx, Class<?> moduleClass) {
        Object catalyst = Reflect.field(ctx, "mCatalystInstance");
        if (Reflect.missing(catalyst) || catalyst == null) return null;
        Object modules = Reflect.field(Reflect.field(catalyst, "mNativeModuleRegistry"), "mModules", "modules");
        String name = Reflect.annotationName(moduleClass, REACT_MODULE);
        if (!(modules instanceof Map) || name == null) return null;
        Object holder = ((Map<?, ?>) modules).get(name);
        if (holder == null) return Boolean.FALSE;
        Object has = Reflect.call(holder, "hasInstance");
        return has instanceof Boolean ? has : null;
    }

    private int unknown(String tracker, String why) {
        ProbeLog.warnOnce("rn-" + tracker, tracker + ": " + why + "; not waited for (React Native version not supported?)");
        return UNKNOWN_RESULT;
    }

    private static final int UNKNOWN_RESULT = Tracker.UNKNOWN;

    // --- trackers ---------------------------------------------------------------

    synchronized int loading(long maxTimerMs) {
        Object ctx = context();
        if (Reflect.missing(ctx)) {
            if (hostNotFound) return UNKNOWN_RESULT; // warned once, in context()
            return isReactApp ? unknown("rn_loading", "no ReactHost / ReactNativeHost on the Application") : UNKNOWN_RESULT;
        }
        long now = clock.uptimeMs();
        if (ctx != null) {
            missingSince = Long.MIN_VALUE;
            return 0;
        }
        // Counted from when the context went missing (first ask, or a reload).
        if (missingSince == Long.MIN_VALUE) missingSince = now;
        if (now - missingSince > LOADING_MAX_MS) {
            return unknown("rn_loading", "no React context " + (LOADING_MAX_MS / 1000) + " s after it went missing");
        }
        return 1;
    }

    synchronized int timers(long maxTimerMs) {
        Object ctx = context();
        if (ctx == null) return 0; // rn_loading says so
        if (Reflect.missing(ctx)) return UNKNOWN_RESULT;
        long horizon = maxTimerMs < 0 ? LooperTimers.DEFAULT_MAX_TIMER_MS : maxTimerMs;
        long now = clock.uptimeMs();
        Object timing = module(ctx, TIMING_MODULE);
        Object manager = Reflect.field(timing, "mJavaTimerManager", "javaTimerManager");
        if (Reflect.missing(manager) || manager == null) manager = bridgelessTimerManager();
        int fromQueue = timerQueue(manager, horizon, now);
        if (fromQueue != UNKNOWN_RESULT) return fromQueue;
        // The queue can't be read: RN's own check (busy or not, no perpetual detection by id).
        Object v = Reflect.call(timing, "hasActiveTimersInRange", horizon);
        if (!(v instanceof Boolean)) v = Reflect.call(manager, "hasActiveTimersInRange", horizon);
        if (!(v instanceof Boolean)) return unknown("rn_timers", "no JavaTimerManager timers or hasActiveTimersInRange");
        return busyStreak((Boolean) v, now, horizon) ? 1 : 0;
    }

    /**
     * Without timer ids: a one-shot timer under the horizon lives at most
     * that long, so busy without a break for twice that (2 s at least) is
     * timers re-armed again and again.
     */
    private boolean busyStreak(boolean busy, long now, long horizon) {
        boolean continuous = timersLastSample != Long.MIN_VALUE && now - timersLastSample <= RepostChains.SAMPLE_GAP_MS;
        timersLastSample = now;
        if (!busy) {
            timersBusySince = Long.MIN_VALUE;
            timersPerpetual = false;
            return false;
        }
        if (timersBusySince == Long.MIN_VALUE || (!continuous && !timersPerpetual)) timersBusySince = now;
        boolean perpetual = now - timersBusySince >= Math.max(PERPETUAL_MS, 2 * horizon);
        timersPerpetual = perpetual;
        if (perpetual) ProbeLog.warnOnce("rn-timers-perpetual", "rn_timers: JS timers re-armed without a break: not waited for");
        return !perpetual;
    }

    /** {@code JavaTimerManager.timers} (a PriorityQueue of Timer) under {@code timerGuard}; UNKNOWN if unreadable. */
    private int timerQueue(Object manager, long horizon, long now) {
        if (Reflect.missing(manager) || manager == null) return UNKNOWN_RESULT;
        Object queue = Reflect.field(manager, "timers", "mTimers");
        Object guard = Reflect.field(manager, "timerGuard", "mTimerGuard");
        if (!(queue instanceof java.util.Collection) || Reflect.missing(guard) || guard == null) return UNKNOWN_RESULT;
        Object[] snapshot;
        synchronized (guard) {
            snapshot = ((java.util.Collection<?>) queue).toArray();
        }
        List<RepostChains.Item> items = new ArrayList<>();
        List<Long> intervals = new ArrayList<>();
        for (Object t : snapshot) {
            Object id = Reflect.field(t, "timerId", "mCallbackID");
            Object target = Reflect.field(t, "targetTime", "mTargetTime");
            Object interval = Reflect.field(t, "interval", "mInterval");
            Object repeat = Reflect.field(t, "repeat", "mRepeat");
            if (!(id instanceof Number) || !(target instanceof Number) || !(interval instanceof Number)
                    || !(repeat instanceof Boolean)) {
                return UNKNOWN_RESULT;
            }
            if ((Boolean) repeat) continue; // setInterval: RN's rule
            long iv = ((Number) interval).longValue();
            items.add(new RepostChains.Item("js#" + id, "js", ((Number) target).longValue(), iv));
            intervals.add(iv);
        }
        List<RepostChains.Verdict> verdicts = timerChains.sample(items, now);
        int n = 0;
        boolean perpetual = false;
        for (int i = 0; i < items.size(); i++) {
            if (intervals.get(i) >= horizon) continue; // RN: interval < range
            if (verdicts.get(i).perpetual) {
                perpetual = true;
                continue;
            }
            n++;
        }
        timersPerpetual = perpetual;
        if (perpetual) {
            ProbeLog.warnOnce("rn-timers-perpetual", "rn_timers: a JS timer re-armed as it fires (polling, requestAnimationFrame): not waited for");
        }
        return n;
    }

    /** ReactHostImpl -> ReactInstance -> JavaTimerManager. */
    private Object bridgelessTimerManager() {
        Object host = existingReactHost();
        Object instance = Reflect.field(host, "mReactInstance", "reactInstance");
        return Reflect.field(instance, "mJavaTimerManager", "javaTimerManager");
    }

    int mounting() {
        Object ctx = context();
        if (ctx == null) return 0;
        if (Reflect.missing(ctx)) return UNKNOWN_RESULT;
        Object fabric = Reflect.call(ctx, "getFabricUIManager");
        if (Reflect.missing(fabric) || fabric == null || Reflect.missing(Reflect.field(fabric, "mMountItemDispatcher", "mountItemDispatcher"))) {
            Object viaHelper = Reflect.callStatic(
                    Reflect.cls(UI_MANAGER_HELPER, ctx.getClass().getClassLoader()), "getUIManager", ctx, FABRIC);
            if (!Reflect.missing(viaHelper) && viaHelper != null) fabric = viaHelper;
        }
        Object dispatcher = Reflect.field(fabric, "mMountItemDispatcher", "mountItemDispatcher");
        if (!Reflect.missing(dispatcher) && dispatcher != null) {
            int items = Reflect.size(Reflect.field(dispatcher, "mMountItems", "mountItems"));
            int commands = Reflect.size(Reflect.field(dispatcher, "mViewCommandMountItems", "viewCommandMountItems"));
            if (items < 0 || commands < 0) return unknown("rn_mounting", "MountItemDispatcher queues not found");
            return items + commands;
        }
        // Old architecture: the UIManagerModule's operation queue.
        Object queue = Reflect.call(Reflect.call(module(ctx, UI_MANAGER_MODULE), "getUIImplementation"),
                "getUIViewOperationQueue");
        Object empty = Reflect.call(queue, "isEmpty");
        if (empty instanceof Boolean) return (Boolean) empty ? 0 : 1;
        return unknown("rn_mounting", "neither Fabric's MountItemDispatcher nor UIViewOperationQueue found");
    }

    synchronized int animation() {
        Object ctx = context();
        if (ctx == null) return animLast = animIdle();
        if (Reflect.missing(ctx)) return UNKNOWN_RESULT;
        Object module = module(ctx, ANIMATED_MODULE);
        if (module == null) return animLast = animIdle(); // never created: no native-driven animation yet
        if (Reflect.missing(module)) return unknown("rn_animation", "no NativeAnimatedModule");
        // The field, not getNodesManager(): that creates the manager.
        Object nodes = Reflect.field(module, "mNodesManager", "nodesManagerRef", "nodesManager");
        if (Reflect.missing(nodes)) return unknown("rn_animation", "no NativeAnimatedModule.mNodesManager / nodesManagerRef");
        if (nodes == null) return animLast = animIdle(); // no native-driven animation ever started
        Object[] read = readNodes(nodes);
        if (read == null) return animLast; // the UI thread didn't answer in time: as last read
        Object active = read[0];
        if (!(active instanceof Boolean)) return unknown("rn_animation", "no NativeAnimatedNodesManager.hasActiveAnimations");
        if (!(Boolean) active) return animLast = animIdle();
        long now = clock.uptimeMs();
        boolean continuous = animLastSample != Long.MIN_VALUE && now - animLastSample <= RepostChains.SAMPLE_GAP_MS;
        animLastSample = now;
        if (animBusySince == Long.MIN_VALUE || (!continuous && !animationPerpetual)) {
            animBusySince = now;
            animRearms = 0;
        }
        @SuppressWarnings("unchecked")
        List<int[]> drivers = (List<int[]>) read[1];
        if (drivers == null) {
            // Can't see the drivers: busy without a break for long is a loop.
            boolean perpetual = now - animBusySince >= ANIMATION_FALLBACK_MS;
            animationPerpetual = perpetual;
            return animLast = perpetual ? 0 : 1;
        }
        Set<Integer> ids = new HashSet<>();
        int looping = 0;
        int other = 0;
        for (int[] d : drivers) {
            ids.add(d[0]);
            Long first = driverSeen.get(d[0]);
            if (first == null) driverSeen.put(d[0], first = now);
            // Only an endless loop: a finite Animated.loop({iterations: n}) ends, so it counts.
            boolean loops = d[1] == -1;
            if (loops && now - first >= PERPETUAL_MS) looping++;
            else other++;
        }
        driverSeen.keySet().retainAll(ids);
        // JS-restarted loops: drivers replaced by new ones without a break.
        if (continuous && !animLastIds.isEmpty() && !animLastIds.containsAll(ids) && !ids.containsAll(animLastIds)) {
            animRearms++;
            animLoopWidth = ids.size();
        }
        animLastIds = ids;
        if (animRearms >= RepostChains.REPOSTS + 1 && now - animBusySince >= PERPETUAL_MS) {
            // Ignore the loop's own drivers; a one-shot started on top still counts.
            other = Math.max(0, other - animLoopWidth);
            looping = Math.max(looping, 1);
        }
        animationPerpetual = looping > 0;
        if (animationPerpetual) {
            ProbeLog.warnOnce("rn-animation-perpetual", "rn_animation: a looping native animation: not waited for");
        }
        // Updated nodes alone (no counted driver) are the loop's own frames.
        return animLast = other;
    }

    /** {hasActiveAnimations, drivers or null}, read on the UI thread; null when it didn't answer in time. */
    private Object[] readNodes(Object nodes) {
        if (uiSampler == null) return readNodesNow(nodes);
        uiNodes = nodes;
        uiResult.set(null);
        if (uiSampler.read(0) != 1) return null;
        return uiResult.getAndSet(null);
    }

    /** On the UI thread. */
    private int readNodesOnUi() {
        Object nodes = uiNodes;
        if (nodes == null) return 0;
        uiResult.set(readNodesNow(nodes));
        return 1;
    }

    private static Object[] readNodesNow(Object nodes) {
        Object active = Reflect.call(nodes, "hasActiveAnimations");
        return new Object[] {active, Boolean.TRUE.equals(active) ? drivers(nodes) : null};
    }

    private int animIdle() {
        animBusySince = Long.MIN_VALUE;
        animLastIds = Collections.emptySet();
        animRearms = 0;
        animationPerpetual = false;
        driverSeen.clear();
        return 0;
    }

    /** {id, iterations, currentLoop} per active driver, or null when they can't be read. */
    private static List<int[]> drivers(Object nodes) {
        Object active = Reflect.field(nodes, "mActiveAnimations", "activeAnimations");
        Object size = Reflect.call(active, "size");
        if (!(size instanceof Integer)) return null;
        List<int[]> out = new ArrayList<>();
        for (int i = 0; i < (Integer) size; i++) {
            Object d = Reflect.call(active, "valueAt", i);
            if (Reflect.missing(d)) return null;
            if (d == null) continue;
            Object id = Reflect.field(d, "id", "mId");
            Object iterations = Reflect.field(d, "iterations", "mIterations");
            Object loop = Reflect.field(d, "currentLoop", "mCurrentLoop");
            out.add(new int[] {
                id instanceof Integer ? (Integer) id : System.identityHashCode(d),
                iterations instanceof Integer ? (Integer) iterations : 1,
                loop instanceof Integer ? (Integer) loop : 1});
        }
        return out;
    }

    synchronized int network() {
        Object ctx = context();
        if (ctx == null) return 0;
        if (Reflect.missing(ctx)) return UNKNOWN_RESULT;
        Object module = module(ctx, NETWORKING_MODULE);
        if (module == null) return 0; // never created: no fetch yet
        Object client = Reflect.field(module, "mClient", "client");
        Object dispatcher = Reflect.call(client, "dispatcher");
        Object running = Reflect.call(dispatcher, "runningCalls");
        Object queued = Reflect.call(dispatcher, "queuedCalls");
        if (!(running instanceof List)) return unknown("rn_network", "no OkHttp client on NetworkingModule");
        long now = clock.uptimeMs();
        Set<Integer> seen = new HashSet<>();
        boolean[] stream = new boolean[1];
        int n = counted((List<?>) running, now, seen, stream);
        if (queued instanceof List) n += counted((List<?>) queued, now, seen, stream);
        callSeen.keySet().retainAll(seen);
        networkPerpetual = stream[0];
        if (stream[0]) {
            ProbeLog.warnOnce("rn-network-stream", "rn_network: a request open over " + (STREAM_MAX_MS / 1000)
                    + " s (a stream?): not waited for; add its URL to ignore_urls");
        }
        return n;
    }

    private int counted(List<?> calls, long now, Set<Integer> seen, boolean[] stream) {
        int n = 0;
        for (Object call : calls.toArray()) {
            Object url = Reflect.call(Reflect.call(call, "request"), "url");
            if (!Reflect.missing(url) && url != null && blocklist.isIgnored(String.valueOf(url))) continue;
            int id = System.identityHashCode(call);
            seen.add(id);
            Long first = callSeen.get(id);
            if (first == null) callSeen.put(id, first = now);
            if (now - first > STREAM_MAX_MS) {
                stream[0] = true;
                continue;
            }
            n++;
        }
        return n;
    }
}
