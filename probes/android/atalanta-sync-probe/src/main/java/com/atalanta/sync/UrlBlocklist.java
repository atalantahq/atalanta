// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

package com.atalanta.sync;

import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.List;
import java.util.regex.Pattern;
import java.util.regex.PatternSyntaxException;

/**
 * URLs whose requests never count as pending work. A WebSocket or a
 * long-poll stays open for the life of the screen, so counting it keeps the
 * app "busy" on every step. EarlGrey ({@code kGREYConfigKeyBlockedURLRegex})
 * and DetoxSync ({@code URLBlacklist}) ship the same escape hatch.
 */
public final class UrlBlocklist {
    /** Persistent-connection patterns ignored by default (regex, case-insensitive, find). */
    public static final List<String> DEFAULT_PATTERNS = Collections.unmodifiableList(Arrays.asList(
            "^wss?://",
            "/socket\\.io/",
            "/sockjs(-node)?/",
            "/signalr/",
            "[?&]transport=(polling|longpoll|websocket)",
            "long[-_]?poll",
            "/cable$"));

    public static final UrlBlocklist SHARED = new UrlBlocklist(DEFAULT_PATTERNS);

    private volatile List<Pattern> patterns = Collections.emptyList();

    public UrlBlocklist(List<String> initial) {
        set(initial);
    }

    /** Replace the list; returns the patterns that did not compile. */
    public synchronized List<String> set(List<String> list) {
        List<String> bad = new ArrayList<>();
        patterns = Collections.unmodifiableList(compile(list, bad));
        return bad;
    }

    /**
     * Add to the list; returns the patterns that did not compile. A pattern
     * already on the list is not added again (the host sends the suite's
     * list on every connection).
     */
    public synchronized List<String> add(List<String> list) {
        List<String> bad = new ArrayList<>();
        List<Pattern> next = new ArrayList<>(patterns);
        for (Pattern p : compile(list, bad)) {
            boolean seen = false;
            for (Pattern q : next) {
                if (q.pattern().equals(p.pattern())) {
                    seen = true;
                    break;
                }
            }
            if (!seen) next.add(p);
        }
        patterns = Collections.unmodifiableList(next);
        return bad;
    }

    /** How many patterns are on the list. */
    public int size() {
        return patterns.size();
    }

    public boolean isIgnored(String url) {
        if (url == null) return false;
        for (Pattern p : patterns) {
            if (p.matcher(url).find()) return true;
        }
        return false;
    }

    private static List<Pattern> compile(List<String> list, List<String> bad) {
        List<Pattern> out = new ArrayList<>();
        for (String s : list) {
            try {
                out.add(Pattern.compile(s, Pattern.CASE_INSENSITIVE));
            } catch (PatternSyntaxException e) {
                bad.add(s);
            }
        }
        return out;
    }
}
