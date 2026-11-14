package org.androidcontroller.keeper;

import android.content.Context;
import android.os.Build;
import android.util.Log;

import org.json.JSONException;
import org.json.JSONObject;

/**
 * Remote access to PCs sharing with Test Bed, on this network or over the
 * internet: Test Bed's own connection code (crates/ac-remote-android), so
 * pairing and the stream are the same as between PCs. Everything except the
 * session's input calls blocks: call it off the main thread.
 */
final class Remote {
    /** Event kinds from {@link #next}: the first byte. */
    static final int EV_STATUS = 0, EV_VIDEO = 1, EV_AUDIO = 2;

    private static Boolean loaded;

    private Remote() {}

    /** Whether this phone can run it (64- or 32-bit ARM, Android 7+). */
    static synchronized boolean available(Context c) {
        if (loaded == null) {
            try {
                System.loadLibrary("testbed_remote");
                init(c.getFilesDir().getAbsolutePath());
                loaded = true;
            } catch (UnsatisfiedLinkError e) {
                Log.w(Keeper.TAG, "remote access unavailable: " + e);
                loaded = false;
            }
        }
        return loaded;
    }

    /** How this phone is named on the PC. */
    static String deviceName() {
        String m = Build.MODEL == null ? "" : Build.MODEL.trim();
        return m.isEmpty() ? "Phone" : m;
    }

    /** The {@code ok} value of a reply; throws with the {@code error} text otherwise. */
    static Object ok(String reply) throws Failed {
        try {
            JSONObject o = new JSONObject(reply);
            if (o.has("error")) throw new Failed(o.getString("error"));
            return o.opt("ok");
        } catch (JSONException e) {
            throw new Failed("bad reply: " + e.getMessage());
        }
    }

    static final class Failed extends Exception {
        Failed(String m) {
            super(m);
        }
    }

    static native void init(String filesDir);

    /** {@code {"ok": [{name, fingerprint, address, internet}]}} */
    static native String hosts();

    /** {@code {"ok": {fingerprint: rtt ms or null}}} */
    static native String probe();

    /** {@code {"ok": [{name, address, fingerprint, pairing}]}}: PCs sharing on this network. */
    static native String discover();

    static native String pairLan(String address, String pin, String name);

    static native String pairInternet(String invite, String name);

    static native String forget(String fingerprint);

    /** A session handle, or 0 ({@link #lastError} says why). */
    static native long connect(String fingerprint, String name, int maxHeight);

    static native String lastError();

    /** The next event (see EV_*), or null after {@code timeoutMs}. */
    static native byte[] next(long h, int timeoutMs);

    /** Pointer position, 0..1 of the picture. */
    static native void motion(long h, float x, float y);

    /** Linux evdev codes: BTN_LEFT = 0x110 and so on. */
    static native void button(long h, int code, boolean pressed);

    static native void key(long h, int code, boolean pressed);

    /** In 1/120ths of a wheel notch; positive y scrolls down. */
    static native void scroll(long h, int x, int y);

    static native void releaseAll(long h);

    /** Puts {@code text} on the PC's clipboard. */
    static native void setClipboard(long h, String text);

    /** From now on the PC's clipboard arrives as {@code clipboard} status events. */
    static native void watchClipboard(long h);

    static native void keyframe(long h);

    /** Ends the session; {@code h} is invalid afterwards. */
    static native void close(long h);
}
