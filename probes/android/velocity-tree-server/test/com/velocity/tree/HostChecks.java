package com.velocity.tree;

/**
 * Checks of Server's pure helpers on the host JVM (no device); build.sh runs
 * them before packaging the jar.
 */
final class HostChecks {
    public static void main(String[] args) {
        // N6: a maxLength cut never splits a surrogate pair.
        String smile = "ab😀c"; // a b 😀 c
        check(Server.cut(smile, 3).equals("ab"), "cut before a high surrogate");
        check(Server.cut(smile, 4).equals("ab😀"), "cut after a pair");
        check(Server.cut(smile, -1).equals(smile), "no limit");
        check(Server.cut(smile, 0).isEmpty(), "zero");
        check(Server.VERSION.equals("19"), "protocol 19");
        // Protocol 19: a resend waits as long as the host does, at most 60 s.
        check(Server.hostWaitMs("ab-1@10000") == 10000, "the host's wait");
        check(Server.hostWaitMs("ab-1") == Server.MAX_RESEND_WAIT_MS, "no wait given");
        check(Server.hostWaitMs("ab-1@999999999") == Server.MAX_RESEND_WAIT_MS, "capped");
        check(Server.hostWaitMs("ab-1@x") == Server.MAX_RESEND_WAIT_MS, "garbage");
        check(Server.hostWaitMs("ab-1@0") == 1, "at least 1 ms");
        // Only the active app's popups (and the framework's) join a dump.
        check(Server.sameApp("com.app", "com.app"), "the app's own popup");
        check(Server.sameApp("com.app", "android"), "a framework popup");
        check(!Server.sameApp("com.app", "com.other"), "another app's window");
        check(!Server.sameApp("com.app", null), "no package");
        videoHandshake();
        gate();
        videoSizes();
        check(Server.ROLE_DESCRIPTION_KEY.equals("AccessibilityNodeInfo.roleDescription"), "role description key");
        // D-53: the Compose delegate's key, verbatim (androidx
        // AndroidComposeViewAccessibilityDelegateCompat ExtraDataTestTagKey).
        check(Server.TEST_TAG_KEY.equals("androidx.compose.ui.semantics.testTag"), "testTag key");
        check(Server.MAX_TAGS > 0, "testTag budget");
        tagReads();
        System.out.println("host checks passed");
    }

    /** Review MAJOR 5: testTag reads are bounded in time and cached. */
    private static void tagReads() {
        TagReads t = new TagReads();
        long now = 1_000_000_000L;
        t.start(now, false);
        check(!t.on() && !t.mayRead(now), "notag/lite: no reads");
        t.start(now, true);
        check(t.on() && t.mayRead(now), "reads on");
        // 50 ms per dump, however many nodes are left.
        check(!t.mayRead(now + TagReads.BUDGET_NS), "budget spent at 50 ms");
        check(TagReads.BUDGET_NS == 50_000_000L, "50 ms budget");
        long a = TagReads.key(3, 77), b = TagReads.key(3, 78), c = TagReads.key(4, 77);
        check(a != b && a != c, "node identity includes the window");
        t.read(a, "row", now);
        t.read(b, null, now);
        check(t.reads == 2, "two binder reads");
        // The next dump serves both from the cache, no binder call.
        t.start(now + 1_000_000L, true);
        check("row".equals(t.cached(a, now + 1_000_000L)), "tag cached across dumps");
        check("".equals(t.cached(b, now + 1_000_000L)), "absence cached too");
        check(t.cached(c, now) == null, "unknown node");
        check(t.cached(a, now + TagReads.TTL_NS + 1) == null, "expires");
        // review-fixes #5: a changed Compose tag is read again within 2 s.
        t.read(a, "row", now);
        check(t.cached(a, now + 2_000_000_001L) == null, "a cached tag lives at most 2 s");
        // At most MAX_READS per dump even with time to spare.
        t.start(now, true);
        for (int i = 0; i < TagReads.MAX_READS; i++) t.read(TagReads.key(9, i), "x", now);
        check(!t.mayRead(now), "count cap");
        // The cache stays bounded.
        for (int i = 0; i < TagReads.MAX_CACHED + 10; i++) {
            t.start(now, true);
            t.read(TagReads.key(10, i), "y", now);
        }
        check(t.cached(TagReads.key(10, 0), now) == null, "eldest evicted");
        check("y".equals(t.cached(TagReads.key(10, TagReads.MAX_CACHED + 9), now)), "newest kept");
    }

    /** R-2: non-protocol input and commands before AUTH close the connection. */
    private static void gate() {
        check(Server.refusal("POST", true) != null, "an HTTP request line closes, even after AUTH");
        check(Server.refusal("GET", false) != null, "GET closes");
        check(Server.refusal("Content-Type:", true) != null, "a header line closes");
        check(Server.refusal("", true) != null, "an empty line closes");
        check(Server.refusal("TAP", false) != null, "TAP needs AUTH");
        check(Server.refusal("TAP", true) == null, "TAP after AUTH runs");
        check(Server.refusal("PING", false) == null, "PING before AUTH");
        check(Server.refusal("QUIT", false) == null, "QUIT before AUTH");
        check(Server.refusal("AUTH", false) == null, "AUTH itself");
        check(Server.refusal("DUMP", false) != null, "DUMP needs AUTH");
        check(Server.refusal("x\r\n" + "y".repeat(100), true).length() < 80, "echo is short and printable");
        check(Server.TOKEN.matches("[0-9a-f]{32}"), "128-bit hex token");
        check(Server.tokenMatches(Server.TOKEN), "token matches");
        check(!Server.tokenMatches(Server.TOKEN.substring(1)), "prefix refused");
        check(!Server.tokenMatches(""), "empty refused");
        check(Server.refusal("HELLO", false) == null, "HELLO before AUTH");
        // Protocol 18: the server proves it holds the token (RFC 4231 case 2
        // pins the HMAC; the host's copy checks the same vector).
        check(Server.hmac("Jefe", "what do ya want for nothing?")
                .equals("5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"), "HMAC-SHA256");
        String cn = "00112233445566778899aabbccddeeff";
        check(Server.validNonce(cn), "a 128-bit nonce");
        check(!Server.validNonce("abc"), "too short");
        check(!Server.validNonce("zz112233445566778899aabbccddeeff"), "not hex");
        String proof = Server.serverProof(cn);
        check(proof.matches("[0-9a-f]{64}"), "proof is hex");
        check(!Server.clientProofMatches(cn, proof), "the server's proof is not the client's");
        check(!Server.clientProofMatches(cn, Server.TOKEN), "the bare token no longer authenticates");
        check(Server.clientProofMatches(cn,
                Server.hmac(Server.TOKEN, "velocity-tree client " + Server.NONCE + " " + cn)), "client proof");
        check(proof.equals(Server.hmac(Server.TOKEN, "velocity-tree server " + Server.NONCE + " " + cn)), "server proof");
    }

    /** VideoServer 2: the mutual handshake's proofs (the host computes the same). */
    private static void videoHandshake() {
        String token = "0123456789abcdef0123456789abcdef";
        String cn = "00112233445566778899aabbccddeeff", sn = "ffeeddccbbaa99887766554433221100";
        check(VideoServer.serverProof(token, cn, sn).equals(
                Server.hmac(token, "velocity-video server " + cn + " " + sn)), "video server proof");
        check(!VideoServer.serverProof(token, cn, sn).equals(VideoServer.clientProof(token, cn, sn)),
                "the server's proof is not the client's");
        check(VideoServer.tokenFile("velocity_video_1_2").equals("/data/local/tmp/velocity_video_1_2.token"),
                "video token file");
        check(VideoServer.VERSION.equals("2"), "video server 2");
    }

    /** VideoServer: long edge capped, both sides multiples of 8. */
    private static void videoSizes() {
        int[] a = VideoServer.videoSize(1080, 2400, 1280);
        check(a[0] == 576 && a[1] == 1280, "portrait capped at 1280: " + a[0] + "x" + a[1]);
        int[] b = VideoServer.videoSize(2400, 1080, 1280);
        check(b[0] == 1280 && b[1] == 576, "landscape capped");
        int[] c = VideoServer.videoSize(1001, 703, 0);
        check(c[0] == 1000 && c[1] == 696, "no cap, rounded down to 8");
        int[] d = VideoServer.videoSize(720, 1280, 1280);
        check(d[0] == 720 && d[1] == 1280, "already small enough");
        check((VideoServer.FLAG_SESSION >>> 32) == 0x80000000L, "session flag is the top bit");
        check(VideoServer.CODEC_H264 == 0x68323634, "h264 codec id");
    }

    private static void check(boolean ok, String what) {
        if (!ok) throw new AssertionError(what);
    }
}
