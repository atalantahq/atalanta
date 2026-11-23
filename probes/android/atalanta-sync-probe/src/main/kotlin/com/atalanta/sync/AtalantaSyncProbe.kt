package com.velocity.sync

import android.content.Context
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.util.Printer
import java.io.File
import java.util.concurrent.CopyOnWriteArrayList
import java.util.concurrent.atomic.AtomicBoolean

/**
 * VelocitySyncProbe: Embedded in debug app builds to provide native idle detection.
 *
 * Usage:
 *   In your Application.onCreate():
 *     if (BuildConfig.DEBUG) {
 *         VelocitySyncProbe.start(this)
 *     }
 *
 * The probe tracks the main looper, delayed main-looper messages due within
 * 1.5 s (`Handler.postDelayed`; see [LooperTimers]), animations and the
 * network requests the app reports to determine when the app is idle. With
 * `framework: react_native` in the suite it also tracks React Native's JS
 * timers, Fabric mounting, native-driven animations and `fetch`
 * ([ReactNativeIdling]); pass a Context to [start] for that. A server on the device's
 * loopback (port 19401, or `VELOCITY_SYNC_PORT`) exposes this state to the
 * Velocity test runner, which reaches it through a per-device `adb forward`
 * and checks the probe's `hello` (package and pid) before trusting it.
 *
 * Network: the probe can't see requests by itself. Report them with the
 * ready-made OkHttp interceptor:
 *
 *     OkHttpClient.Builder().addInterceptor(VelocityOkHttpInterceptor()).build()
 *
 * or call [networkStarted] / [networkFinished] from your own HTTP layer.
 *
 * WebSocket and long-poll URLs never count (see [UrlBlocklist.DEFAULT_PATTERNS]);
 * add your own with [ignoreUrls].
 */
object VelocitySyncProbe {
    private val stateTracker = AppStateTracker
    private var server: SyncServer? = null
    private val resources = mutableListOf<IdlingResource>()
    private var started = AtomicBoolean(false)

    /**
     * Start the probe on the given port (default `VELOCITY_SYNC_PORT`, else 19401).
     * Call from main thread in debug builds only.
     */
    @JvmStatic
    @JvmOverloads
    fun start(port: Int = SyncServer.resolvePort(System.getenv())) = start(null, port)

    /**
     * As [start], naming the app by `context.packageName`. `hello` reports
     * the package (what the runner checks) and the process separately: an
     * app with `android:process` used to report its process and be refused.
     */
    @JvmStatic
    @JvmOverloads
    fun start(context: Context?, port: Int = SyncServer.resolvePort(System.getenv())) {
        if (!started.compareAndSet(false, true)) return

        // Register built-in idling resources
        register(MainLooperIdlingResource())
        register(AnimationIdlingResource())
        Trackers.SHARED.setApplication(context?.applicationContext)
        Trackers.SHARED.add(mainLooperTimers())

        val process = processName()
        val pkg = context?.packageName ?: currentPackageName() ?: process.substringBefore(':')
        server = SyncServer(stateTracker, port, pkg, process, android.os.Process.myPid())
            .also { it.start() }
    }

    /** Stop the probe and release all resources. */
    @JvmStatic
    fun stop() {
        if (!started.compareAndSet(true, false)) return
        server?.stop()
        server = null
        for (resource in resources) stateTracker.unregisterResource(resource)
        resources.clear()
        Trackers.SHARED.clear()
    }

    /**
     * Track React Native's work without waiting for the host's `config`
     * (it sends `framework: react_native` itself when the suite says so).
     * May be called before [start]: tracking then begins once [start] is
     * given a Context.
     */
    @JvmStatic
    fun trackReactNative() {
        Trackers.SHARED.enableFramework(Trackers.REACT_NATIVE)
    }

    /**
     * Delayed messages whose callback class (for a message without a
     * callback: its Handler class) starts with one of these never count as
     * pending timers (added to [LooperTimers.DEFAULT_IGNORED]). Process-wide:
     * takes effect whether called before or after [start], and survives
     * [stop] / [start].
     */
    @JvmStatic
    fun ignoreTimerCallbacks(prefixes: List<String>) {
        LooperTimers.ignoreCallbacksGlobally(prefixes)
    }

    /**
     * Trackers that never count (`timer`, `rn_loading`, `rn_timers`,
     * `rn_mounting`, `rn_animation`, `rn_network`): the app-side form of
     * `config` `ignore_trackers`. Replaces the last list; returns the names
     * the probe doesn't have.
     */
    @JvmStatic
    fun ignoreTrackers(names: List<String>): List<String> = Trackers.SHARED.optOut(names)

    /** The main looper's delayed messages: its queue walked, else `Looper.dump`. */
    private fun mainLooperTimers(): LooperTimers {
        val looper = Looper.getMainLooper()
        val queue: Any? = try {
            if (android.os.Build.VERSION.SDK_INT >= 23) looper.queue else null
        } catch (_: Throwable) {
            null
        }
        return LooperTimers(
            queue,
            { out -> looper.dump(Printer { line -> out.line(line) }, "") },
            { SystemClock.uptimeMillis() },
        )
    }

    /** Whether the app is currently idle. */
    @JvmStatic
    val isIdle: Boolean
        get() = stateTracker.idleNow()

    /** Register a custom IdlingResource. */
    @JvmStatic
    fun register(resource: IdlingResource) {
        resources.add(resource)
        stateTracker.registerResource(resource)
    }

    /**
     * A request to [url] started. Returns whether it counts as pending work
     * (blocklisted URLs don't); pass the result to [networkFinished].
     */
    @JvmStatic
    fun networkStarted(url: String): Boolean = NetworkTracker.SHARED.started(url)

    /** A request ended; [tracked] is what [networkStarted] returned. */
    @JvmStatic
    fun networkFinished(tracked: Boolean) = NetworkTracker.SHARED.finished(tracked)

    /**
     * Requests to URLs matching these regular expressions never hold the app
     * busy (added to the WebSocket/long-poll defaults). Returns the patterns
     * that are not valid regular expressions.
     */
    @JvmStatic
    fun ignoreUrls(patterns: List<String>): List<String> = UrlBlocklist.SHARED.add(patterns)

    /** The process name (the package unless `android:process` says otherwise). */
    private fun processName(): String = try {
        File("/proc/self/cmdline").readText().trim('\u0000', ' ').substringBefore('\u0000')
    } catch (_: Exception) {
        ""
    }

    /** The app's package, from the hidden `ActivityThread.currentPackageName()`. */
    private fun currentPackageName(): String? = try {
        Class.forName("android.app.ActivityThread").getMethod("currentPackageName")
            .invoke(null) as? String
    } catch (_: Throwable) {
        null
    }
}

/** Interface for custom idling resources (Espresso-compatible pattern). */
interface IdlingResource {
    /** Human-readable name for debugging. */
    val name: String

    /** Whether this resource is currently idle. */
    val isIdle: Boolean
}

/**
 * A resource that may be unable to tell on this device (a hidden framework
 * API it can't reach). An unknown resource doesn't hold the app busy, but the
 * probe reports it in `unknown` instead of claiming it idle.
 */
interface UncertainIdlingResource : IdlingResource {
    val isUnknown: Boolean
}

/** Tracks the aggregate idle state from all registered resources. */
object AppStateTracker : ProbeState {
    private val _customResources = CopyOnWriteArrayList<IdlingResource>()

    val isIdle: Boolean
        get() = NetworkTracker.SHARED.count() == 0 && _customResources.all { it.isIdle }

    val pendingReasons: List<String>
        get() {
            val reasons = mutableListOf<String>()
            val net = NetworkTracker.SHARED.count()
            if (net > 0) reasons.add("network:$net")
            for (resource in _customResources) {
                if (!resource.isIdle) reasons.add(resource.name)
            }
            return reasons
        }

    /** Trackers that can't tell on this device. */
    val unknownReasons: List<String>
        get() = _customResources
            .filter { it is UncertainIdlingResource && it.isUnknown }
            .map { it.name }

    fun incrementNetwork() { NetworkTracker.SHARED.increment() }
    fun decrementNetwork() { NetworkTracker.SHARED.decrement() }

    fun registerResource(resource: IdlingResource) {
        _customResources.add(resource)
    }

    fun unregisterResource(resource: IdlingResource) {
        _customResources.remove(resource)
    }

    override fun idleNow(): Boolean = idleNow(-1)
    override fun pendingNow(): List<String> = pendingNow(-1)

    // Main-looper timers and React Native's queues take the connection's horizon.
    override fun idleNow(maxTimerMs: Long): Boolean = isIdle && Trackers.SHARED.idle(maxTimerMs)
    override fun pendingNow(maxTimerMs: Long): List<String> =
        pendingReasons + Trackers.SHARED.pending(maxTimerMs)
    override fun unknownTrackers(): List<String> = unknownReasons + Trackers.SHARED.unknown()
    override fun enableFramework(framework: String): Boolean = Trackers.SHARED.enableFramework(framework)
}

/**
 * Whether the main thread is idle, asked on the main thread (see
 * [MainThreadCheck]): `MessageQueue.isIdle` read from the server thread was
 * true while the main thread ran a long message, so jank read as idle.
 */
class MainLooperIdlingResource : IdlingResource {
    override val name = "main_looper"

    private val mainLooper = Looper.getMainLooper()
    private val handler = Handler(mainLooper)
    private val check = MainThreadCheck(
        { r -> handler.postAtFrontOfQueue(r) },
        {
            if (android.os.Build.VERSION.SDK_INT >= 23) mainLooper.queue.isIdle else true
        },
    )

    override val isIdle: Boolean
        get() = if (Looper.myLooper() == mainLooper) {
            // Asked on the main thread itself: it is running this, nothing else.
            android.os.Build.VERSION.SDK_INT < 23 || mainLooper.queue.isIdle
        } else {
            check.isIdle()
        }
}

/**
 * Running animators through the framework's hidden counter (see
 * [AnimationCounter]). When it can't be read the resource is *unknown*, not
 * idle: it no longer holds the app busy, and the probe says so.
 *
 * The counter is per-thread (`AnimationHandler` is a `ThreadLocal`), so it is
 * read on the main thread: from the server thread it was always 0. A main
 * thread that can't answer within the budget reads busy.
 */
class AnimationIdlingResource : UncertainIdlingResource {
    override val name = "animation"

    private val mainLooper = Looper.getMainLooper()
    private val handler = Handler(mainLooper)
    private val counter = AnimationCounter.forFramework { r ->
        if (Looper.myLooper() == mainLooper) {
            r.run()
            true
        } else {
            handler.postAtFrontOfQueue(r)
        }
    }

    override val isIdle: Boolean
        get() = counter.count() <= 0

    override val isUnknown: Boolean
        get() = counter.count() == AnimationCounter.UNKNOWN
}
