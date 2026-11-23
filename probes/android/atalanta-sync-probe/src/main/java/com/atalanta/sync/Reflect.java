package com.atalanta.sync;

import java.lang.reflect.Field;
import java.lang.reflect.Method;
import java.util.Map;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.atomic.AtomicReference;

/**
 * Defensive reflection for framework internals the probe reads (hidden
 * Android fields, React Native classes). Nothing here throws: a missing
 * class, method or field, or one that threw, is {@link #MISSING}, which the
 * caller turns into "unknown", never into "idle".
 */
final class Reflect {
    /** No such member, or it threw. Distinct from a member that returned null. */
    static final Object MISSING = new Object() {
        @Override
        public String toString() {
            return "MISSING";
        }
    };

    private static final Object NONE = new Object();
    private static final Map<String, Object> METHODS = new ConcurrentHashMap<>();
    private static final Map<String, Object> FIELDS = new ConcurrentHashMap<>();

    private Reflect() {}

    static boolean missing(Object v) {
        return v == MISSING;
    }

    /** The class, or null; loaded with {@code loader} when given. */
    static Class<?> cls(String name, ClassLoader loader) {
        try {
            return loader == null ? Class.forName(name) : Class.forName(name, false, loader);
        } catch (Throwable t) {
            return null;
        }
    }

    /** {@code target.name(args)}: the first method with that name and arity whose parameters take the args. */
    static Object call(Object target, String name, Object... args) {
        if (target == null || target == MISSING) return MISSING;
        Method m = method(target.getClass(), name, args);
        if (m == null) return MISSING;
        try {
            return m.invoke(target, args);
        } catch (Throwable t) {
            return MISSING;
        }
    }

    /** A static {@code cls.name(args)}. */
    static Object callStatic(Class<?> cls, String name, Object... args) {
        if (cls == null) return MISSING;
        Method m = method(cls, name, args);
        if (m == null) return MISSING;
        try {
            return m.invoke(null, args);
        } catch (Throwable t) {
            return MISSING;
        }
    }

    /**
     * The first of {@code names} declared on {@code target}'s class or a
     * superclass (Java's {@code mFoo} and Kotlin's {@code foo} spellings).
     * An {@code AtomicReference} is read through.
     */
    static Object field(Object target, String... names) {
        if (target == null || target == MISSING) return MISSING;
        for (String name : names) {
            Field f = field(target.getClass(), name);
            if (f == null) continue;
            try {
                Object v = f.get(target);
                return v instanceof AtomicReference ? ((AtomicReference<?>) v).get() : v;
            } catch (Throwable t) {
                return MISSING;
            }
        }
        return MISSING;
    }

    /** A static field's value ({@code object} singletons' private fields), or MISSING. */
    static Object staticField(Class<?> cls, String name) {
        if (cls == null) return MISSING;
        Field f = field(cls, name);
        if (f == null || !java.lang.reflect.Modifier.isStatic(f.getModifiers())) return MISSING;
        try {
            return f.get(null);
        } catch (Throwable t) {
            return MISSING;
        }
    }

    /** The {@code name()} of an annotation of {@code cls} whose type is called {@code annotation}, or null. */
    static String annotationName(Class<?> cls, String annotation) {
        try {
            for (java.lang.annotation.Annotation a : cls.getAnnotations()) {
                if (a.annotationType().getName().equals(annotation)) {
                    Object v = a.annotationType().getMethod("name").invoke(a);
                    return v instanceof String ? (String) v : null;
                }
            }
        } catch (Throwable t) {
            // no name
        }
        return null;
    }

    /** The field on {@code cls} or a superclass, accessible, or null. */
    static Field field(Class<?> cls, String name) {
        String key = cls.getName() + "#" + name + "@" + System.identityHashCode(cls);
        Object cached = FIELDS.get(key);
        if (cached != null) return cached == NONE ? null : (Field) cached;
        Field found = null;
        for (Class<?> c = cls; c != null && found == null; c = c.getSuperclass()) {
            try {
                Field f = c.getDeclaredField(name);
                f.setAccessible(true);
                found = f;
            } catch (Throwable t) {
                // superclass next
            }
        }
        FIELDS.put(key, found == null ? NONE : found);
        return found;
    }

    private static Method method(Class<?> cls, String name, Object[] args) {
        StringBuilder key = new StringBuilder(cls.getName()).append('#').append(name)
                .append('@').append(System.identityHashCode(cls));
        for (Object a : args) key.append(',').append(a == null ? "null" : a.getClass().getName());
        String k = key.toString();
        Object cached = METHODS.get(k);
        if (cached != null) return cached == NONE ? null : (Method) cached;
        Method found = find(cls, name, args);
        METHODS.put(k, found == null ? NONE : found);
        return found;
    }

    private static Method find(Class<?> cls, String name, Object[] args) {
        try {
            for (Method m : cls.getMethods()) {
                if (fits(m, name, args)) return accessible(m);
            }
        } catch (Throwable t) {
            // declared methods next
        }
        for (Class<?> c = cls; c != null; c = c.getSuperclass()) {
            try {
                for (Method m : c.getDeclaredMethods()) {
                    if (fits(m, name, args)) return accessible(m);
                }
            } catch (Throwable t) {
                // superclass next
            }
        }
        return null;
    }

    private static Method accessible(Method m) {
        try {
            m.setAccessible(true);
        } catch (Throwable t) {
            // public methods still work
        }
        return m;
    }

    /**
     * Whether {@code m} is {@code name}: exactly, or a Kotlin {@code internal}
     * member, which compiles to {@code name$<module>} (e.g. RN's
     * {@code hasActiveTimersInRange$ReactAndroid_release}).
     */
    static boolean named(Method m, String name) {
        String n = m.getName();
        return n.equals(name) || (n.startsWith(name) && n.length() > name.length() && n.charAt(name.length()) == '$');
    }

    private static boolean fits(Method m, String name, Object[] args) {
        if (!named(m, name)) return false;
        Class<?>[] params = m.getParameterTypes();
        if (params.length != args.length) return false;
        for (int i = 0; i < params.length; i++) {
            if (!takes(params[i], args[i])) return false;
        }
        return true;
    }

    private static boolean takes(Class<?> param, Object arg) {
        if (arg == null) return !param.isPrimitive();
        if (!param.isPrimitive()) return param.isInstance(arg);
        if (param == long.class) return arg instanceof Long || arg instanceof Integer;
        if (param == int.class) return arg instanceof Integer;
        if (param == boolean.class) return arg instanceof Boolean;
        if (param == double.class) return arg instanceof Double;
        return false;
    }

    /** {@code Collection.size()} / an array's length, or -1. */
    static int size(Object v) {
        if (v instanceof java.util.Collection) return ((java.util.Collection<?>) v).size();
        if (v instanceof Object[]) return ((Object[]) v).length;
        return -1;
    }
}
