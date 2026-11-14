package org.androidcontroller.keeper;

import android.app.Activity;
import android.content.ClipData;
import android.content.ClipboardManager;
import android.content.Context;
import android.content.Intent;
import android.content.pm.ActivityInfo;
import android.graphics.Color;
import android.graphics.Rect;
import android.graphics.drawable.GradientDrawable;
import android.media.AudioAttributes;
import android.media.AudioFormat;
import android.media.AudioTrack;
import android.media.MediaCodec;
import android.media.MediaFormat;
import android.os.Bundle;
import android.os.Handler;
import android.os.Looper;
import android.text.InputType;
import android.util.Log;
import android.view.Gravity;
import android.view.KeyEvent;
import android.view.MotionEvent;
import android.view.Surface;
import android.view.SurfaceHolder;
import android.view.SurfaceView;
import android.view.View;
import android.view.ViewConfiguration;
import android.view.Window;
import android.view.WindowManager;
import android.view.inputmethod.BaseInputConnection;
import android.view.inputmethod.EditorInfo;
import android.view.inputmethod.InputConnection;
import android.view.inputmethod.InputMethodManager;
import android.widget.Button;
import android.widget.FrameLayout;
import android.widget.HorizontalScrollView;
import android.widget.LinearLayout;
import android.widget.TextView;
import android.widget.Toast;

import org.json.JSONException;
import org.json.JSONObject;

import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.util.HashSet;
import java.util.Set;

/**
 * Viewing and using a PC that shares with Test Bed. The picture is the PC's
 * screen: tap to click, hold for a right click, drag to drag, two fingers to
 * scroll (a two-finger tap right-clicks too). Swiping up from the bottom
 * edge opens the keyboard, which types on the PC, with a row of PC keys
 * above it. The clipboard is shared both ways while it is open.
 */
public final class RemoteActivity extends Activity {
    static final String EXTRA_FINGERPRINT = "fingerprint";
    static final String EXTRA_NAME = "name";

    /** A swipe starting this close to the bottom (fraction of the height) opens the keyboard… */
    private static final float EDGE = 0.08f;
    /** …once it has moved up this far. */
    private static final float SWIPE = 0.10f;
    private static final long MIN_MOVE_GAP_MS = 8;
    private static final long TWO_FINGER_TAP_MS = 300;
    private static final long BACK_AGAIN_MS = 2_000;

    private final Handler main = new Handler(Looper.getMainLooper());
    private String fingerprint;
    private String pcName;
    private FrameLayout root;
    private SurfaceView video;
    private TextView statusView;
    private TextView noticeView;
    private Typing typing;
    private HorizontalScrollView strip;
    private final Set<Integer> latched = new HashSet<>();
    private final java.util.Map<Integer, Button> modButtons = new java.util.HashMap<>();
    private boolean warnedChar;
    private long backAt;

    private volatile long session;
    /** The picture is showing: the status line is cleared. */
    private boolean showing;
    private Thread netThread;
    private final Decoder decoder = new Decoder();
    private final AudioOut audio = new AudioOut();

    private int videoW, videoH;
    private ClipboardManager clipboard;
    /** The last clipboard text either way, so nothing echoes back. */
    private volatile String lastClip;
    /** When the phone's clipboard last went to the PC. */
    private long clipSentAt;
    private final ClipboardManager.OnPrimaryClipChangedListener clipChanged = this::sendClipboard;
    /** How much of the window the phone's keyboard covers. */
    private int imeHeight;

    @Override
    protected void onCreate(Bundle saved) {
        super.onCreate(saved);
        fingerprint = getIntent().getStringExtra(EXTRA_FINGERPRINT);
        pcName = getIntent().getStringExtra(EXTRA_NAME);
        if (fingerprint == null || !Remote.available(this)) {
            finish();
            return;
        }
        requestWindowFeature(Window.FEATURE_NO_TITLE);
        Window w = getWindow();
        w.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON | WindowManager.LayoutParams.FLAG_FULLSCREEN);
        // The keyboard covers the picture's bottom instead of squeezing it.
        w.setSoftInputMode(WindowManager.LayoutParams.SOFT_INPUT_ADJUST_NOTHING);
        if (Keeper.isAtLeast(28)) {
            WindowManager.LayoutParams lp = w.getAttributes();
            lp.layoutInDisplayCutoutMode = WindowManager.LayoutParams.LAYOUT_IN_DISPLAY_CUTOUT_MODE_SHORT_EDGES;
            w.setAttributes(lp);
        }
        setRequestedOrientation(ActivityInfo.SCREEN_ORIENTATION_SENSOR_LANDSCAPE);

        root = new FrameLayout(this);
        root.setBackgroundColor(Color.BLACK);
        root.addOnLayoutChangeListener((v, l, t, r, b, ol, ot, or, ob) -> {
            if (r - l != or - ol || b - t != ob - ot) v.post(this::fitVideo);
        });
        if (Keeper.isAtLeast(30)) {
            // Full screen, the visible frame doesn't shrink for the keyboard; its inset does.
            root.setOnApplyWindowInsetsListener((v, insets) -> {
                imeHeight = insets.getInsets(android.view.WindowInsets.Type.ime()).bottom;
                placeStrip();
                return insets;
            });
        } else {
            root.getViewTreeObserver().addOnGlobalLayoutListener(() -> {
                Rect r = new Rect();
                root.getWindowVisibleDisplayFrame(r);
                imeHeight = root.getRootView().getHeight() - r.bottom;
                placeStrip();
            });
        }

        typing = new Typing(this);
        root.addView(typing, new FrameLayout.LayoutParams(1, 1));

        video = new SurfaceView(this);
        video.getHolder().addCallback(new SurfaceHolder.Callback() {
            @Override
            public void surfaceCreated(SurfaceHolder h) {
                decoder.setSurface(h.getSurface());
            }

            @Override
            public void surfaceChanged(SurfaceHolder h, int format, int width, int height) {}

            @Override
            public void surfaceDestroyed(SurfaceHolder h) {
                decoder.setSurface(null);
            }
        });
        root.addView(video, new FrameLayout.LayoutParams(FrameLayout.LayoutParams.MATCH_PARENT,
                FrameLayout.LayoutParams.MATCH_PARENT, Gravity.CENTER));

        statusView = new TextView(this);
        statusView.setTextColor(Color.WHITE);
        statusView.setTextSize(18);
        statusView.setGravity(Gravity.CENTER);
        statusView.setPadding(dp(24), 0, dp(24), 0);
        root.addView(statusView, new FrameLayout.LayoutParams(FrameLayout.LayoutParams.WRAP_CONTENT,
                FrameLayout.LayoutParams.WRAP_CONTENT, Gravity.CENTER));

        noticeView = new TextView(this);
        noticeView.setTextColor(Color.WHITE);
        noticeView.setTextSize(14);
        noticeView.setPadding(dp(12), dp(6), dp(12), dp(6));
        noticeView.setBackground(rounded(0xCC000000, dp(8)));
        noticeView.setVisibility(View.GONE);
        FrameLayout.LayoutParams nlp = new FrameLayout.LayoutParams(FrameLayout.LayoutParams.WRAP_CONTENT,
                FrameLayout.LayoutParams.WRAP_CONTENT, Gravity.TOP | Gravity.CENTER_HORIZONTAL);
        nlp.topMargin = dp(12);
        root.addView(noticeView, nlp);

        strip = makeStrip();
        strip.setVisibility(View.GONE);
        root.addView(strip, new FrameLayout.LayoutParams(FrameLayout.LayoutParams.MATCH_PARENT,
                FrameLayout.LayoutParams.WRAP_CONTENT, Gravity.BOTTOM));

        setContentView(root);
        clipboard = getSystemService(ClipboardManager.class);
        if (clipboard != null) clipboard.addPrimaryClipChangedListener(clipChanged);
        setStatus("Connecting to " + pcName + "…");
        start();
    }

    @Override
    protected void onResume() {
        super.onResume();
        hideSystemBars();
    }

    @Override
    public void onWindowFocusChanged(boolean hasFocus) {
        super.onWindowFocusChanged(hasFocus);
        if (hasFocus) {
            hideSystemBars();
            // Something copied in another app: Android lets it be read now.
            main.post(this::sendClipboard);
        } else if (session != 0) {
            Remote.releaseAll(session);
        }
    }

    @Override
    protected void onDestroy() {
        if (clipboard != null) clipboard.removePrimaryClipChangedListener(clipChanged);
        stop();
        super.onDestroy();
    }

    @Override
    public void onBackPressed() {
        long now = System.currentTimeMillis();
        if (now - backAt < BACK_AGAIN_MS) {
            finish();
            return;
        }
        backAt = now;
        Toast.makeText(this, "Press back again to disconnect", Toast.LENGTH_SHORT).show();
    }

    @SuppressWarnings("deprecation")
    private void hideSystemBars() {
        getWindow().getDecorView().setSystemUiVisibility(View.SYSTEM_UI_FLAG_IMMERSIVE_STICKY
                | View.SYSTEM_UI_FLAG_HIDE_NAVIGATION | View.SYSTEM_UI_FLAG_FULLSCREEN
                | View.SYSTEM_UI_FLAG_LAYOUT_STABLE | View.SYSTEM_UI_FLAG_LAYOUT_HIDE_NAVIGATION
                | View.SYSTEM_UI_FLAG_LAYOUT_FULLSCREEN);
    }

    // ---- the session ------------------------------------------------------------------------

    private void start() {
        String name = Remote.deviceName();
        int maxHeight = Math.min(1080, Math.max(720, getResources().getDisplayMetrics().heightPixels));
        netThread = new Thread(() -> {
            long h = Remote.connect(fingerprint, name, maxHeight);
            if (h == 0) {
                String why = Remote.lastError();
                main.post(() -> setStatus("Can't connect to " + pcName + ": " + why));
                return;
            }
            session = h;
            Remote.watchClipboard(h);
            main.post(this::sendClipboard);
            try {
                while (!Thread.currentThread().isInterrupted()) {
                    byte[] ev = Remote.next(h, 250);
                    if (ev == null || ev.length == 0) continue;
                    switch (ev[0]) {
                        case Remote.EV_VIDEO:
                            if (!showing) {
                                showing = true;
                                main.post(() -> setStatus(""));
                            }
                            decoder.frame(ev, ev[1] != 0, h);
                            break;
                        case Remote.EV_AUDIO:
                            audio.packet(ev);
                            break;
                        default:
                            if (!onStatus(new String(ev, 1, ev.length - 1, java.nio.charset.StandardCharsets.UTF_8))) return;
                            break;
                    }
                }
            } finally {
                decoder.release();
                audio.release();
            }
        }, "remote-session");
        netThread.start();
    }

    private void stop() {
        long h = session;
        session = 0;
        if (netThread != null) {
            netThread.interrupt();
            try {
                netThread.join(1_000);
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
            }
            netThread = null;
        }
        if (h != 0) Remote.close(h);
    }

    /** Handles a status event on the session thread; false once the session is over. */
    private boolean onStatus(String json) {
        JSONObject o;
        try {
            o = new JSONObject(json);
        } catch (JSONException e) {
            return true;
        }
        String text = o.optString("text");
        switch (o.optString("t")) {
            case "connecting":
                showing = false;
                if (o.optInt("attempt") <= 1) main.post(() -> setStatus("Connecting to " + pcName + "…"));
                break;
            case "connected":
                main.post(() -> setStatus("Waiting for the picture…"));
                break;
            case "stream":
                int w = o.optInt("width"), h = o.optInt("height");
                decoder.size(w, h);
                main.post(() -> {
                    videoW = w;
                    videoH = h;
                    fitVideo();
                });
                break;
            case "notice":
                main.post(() -> notice(text));
                break;
            case "clipboard":
                main.post(() -> fromPc(text));
                break;
            case "reconnecting":
                showing = false;
                main.post(() -> setStatus("Reconnecting… (" + text + ")"));
                break;
            case "disconnected":
                // A reconnect follows by itself.
                showing = false;
                main.post(() -> setStatus("Connection lost: " + text));
                break;
            case "fatal":
                main.post(() -> setStatus(text.isEmpty() ? "Disconnected" : text));
                return false;
            case "closed":
                return false;
            default:
                break;
        }
        return true;
    }

    private void setStatus(String text) {
        statusView.setText(text);
        statusView.setVisibility(text.isEmpty() ? View.GONE : View.VISIBLE);
    }

    private final Runnable hideNotice = () -> noticeView.setVisibility(View.GONE);

    private void notice(String text) {
        noticeView.setText(text);
        noticeView.setVisibility(View.VISIBLE);
        main.removeCallbacks(hideNotice);
        main.postDelayed(hideNotice, 4_000);
    }

    /** Keeps the PC screen's shape. */
    private void fitVideo() {
        int rw = root.getWidth(), rh = root.getHeight();
        if (videoW <= 0 || videoH <= 0 || rw <= 0 || rh <= 0) return;
        float scale = Math.min((float) rw / videoW, (float) rh / videoH);
        FrameLayout.LayoutParams lp = (FrameLayout.LayoutParams) video.getLayoutParams();
        int w = Math.round(videoW * scale), h = Math.round(videoH * scale);
        if (lp.width == w && lp.height == h) return;
        lp.width = w;
        lp.height = h;
        video.setLayoutParams(lp);
    }

    // ---- touch: the PC's mouse ---------------------------------------------------------------

    private enum Mode { NONE, PENDING, DRAG, SCROLL, DONE }

    private Mode mode = Mode.NONE;
    private boolean fromEdge;
    private float downX, downY, lastAvgX, lastAvgY;
    private float scrollRemX, scrollRemY;
    private long twoFingersAt;
    private boolean scrolled;
    private long lastMoveSent;
    private final Runnable longPress = () -> {
        if (mode == Mode.PENDING && !fromEdge) {
            mode = Mode.DONE;
            click(PcKeys.BTN_RIGHT);
        }
    };

    @Override
    public boolean onTouchEvent(MotionEvent e) {
        long h = session;
        int slop = ViewConfiguration.get(this).getScaledTouchSlop();
        int height = root.getHeight();
        switch (e.getActionMasked()) {
            case MotionEvent.ACTION_DOWN:
                downX = e.getX();
                downY = e.getY();
                fromEdge = downY > height * (1 - EDGE);
                mode = Mode.PENDING;
                if (!fromEdge) moveTo(e.getX(), e.getY());
                main.postDelayed(longPress, ViewConfiguration.getLongPressTimeout());
                break;
            case MotionEvent.ACTION_POINTER_DOWN:
                main.removeCallbacks(longPress);
                if (mode == Mode.DRAG && h != 0) Remote.button(h, PcKeys.BTN_LEFT, false);
                if (mode == Mode.PENDING || mode == Mode.DRAG) {
                    mode = Mode.SCROLL;
                    twoFingersAt = e.getEventTime();
                    scrolled = false;
                    scrollRemX = scrollRemY = 0;
                    lastAvgX = avg(e, true);
                    lastAvgY = avg(e, false);
                }
                break;
            case MotionEvent.ACTION_MOVE:
                if (mode == Mode.PENDING) {
                    if (fromEdge) {
                        if (downY - e.getY() > height * SWIPE) {
                            main.removeCallbacks(longPress);
                            mode = Mode.DONE;
                            typing.show();
                        }
                        break;
                    }
                    if (Math.hypot(e.getX() - downX, e.getY() - downY) > slop) {
                        main.removeCallbacks(longPress);
                        mode = Mode.DRAG;
                        if (h != 0) Remote.button(h, PcKeys.BTN_LEFT, true);
                    }
                }
                if (mode == Mode.DRAG) {
                    long now = e.getEventTime();
                    if (now - lastMoveSent >= MIN_MOVE_GAP_MS) {
                        lastMoveSent = now;
                        moveTo(e.getX(), e.getY());
                    }
                } else if (mode == Mode.SCROLL && e.getPointerCount() >= 2) {
                    scroll(avg(e, true), avg(e, false), slop);
                }
                break;
            case MotionEvent.ACTION_POINTER_UP:
                if (mode == Mode.SCROLL) {
                    if (!scrolled && e.getEventTime() - twoFingersAt < TWO_FINGER_TAP_MS) click(PcKeys.BTN_RIGHT);
                    mode = Mode.DONE;
                }
                break;
            case MotionEvent.ACTION_UP:
                main.removeCallbacks(longPress);
                if (mode == Mode.PENDING) {
                    // A tap; one near the edge is a click there after all.
                    if (fromEdge) moveTo(e.getX(), e.getY());
                    click(PcKeys.BTN_LEFT);
                } else if (mode == Mode.DRAG) {
                    moveTo(e.getX(), e.getY());
                    if (h != 0) Remote.button(h, PcKeys.BTN_LEFT, false);
                }
                mode = Mode.NONE;
                break;
            case MotionEvent.ACTION_CANCEL:
                main.removeCallbacks(longPress);
                if (mode == Mode.DRAG && h != 0) Remote.button(h, PcKeys.BTN_LEFT, false);
                mode = Mode.NONE;
                break;
            default:
                break;
        }
        return true;
    }

    private static float avg(MotionEvent e, boolean x) {
        float s = 0;
        int n = Math.min(2, e.getPointerCount());
        for (int i = 0; i < n; i++) s += x ? e.getX(i) : e.getY(i);
        return s / n;
    }

    /** Fingers moving up scroll the page down, as on the phone itself. */
    private void scroll(float ax, float ay, int slop) {
        long h = session;
        float dx = ax - lastAvgX, dy = ay - lastAvgY;
        lastAvgX = ax;
        lastAvgY = ay;
        if (!scrolled && Math.abs(dx) + Math.abs(dy) < slop / 4f) return;
        scrolled = true;
        float notch = dp(40);
        scrollRemX += -dx * 120 / notch;
        scrollRemY += -dy * 120 / notch;
        int sx = (int) scrollRemX, sy = (int) scrollRemY;
        if ((sx != 0 || sy != 0) && h != 0) {
            scrollRemX -= sx;
            scrollRemY -= sy;
            Remote.scroll(h, sx, sy);
        }
    }

    private void moveTo(float x, float y) {
        long h = session;
        if (h == 0) return;
        int[] at = new int[2];
        video.getLocationInWindow(at);
        float w = Math.max(1, video.getWidth()), vh = Math.max(1, video.getHeight());
        Remote.motion(h, clamp((x - at[0]) / w), clamp((y - at[1]) / vh));
    }

    private void click(int button) {
        long h = session;
        if (h == 0) return;
        Remote.button(h, button, true);
        Remote.button(h, button, false);
    }

    private static float clamp(float v) {
        return Math.max(0f, Math.min(1f, v));
    }

    // ---- the clipboard -----------------------------------------------------------------------

    /** The phone's clipboard to the PC, if it changed. Main thread. */
    private void sendClipboard() {
        long h = session;
        if (h == 0 || clipboard == null) return;
        String text;
        try {
            ClipData d = clipboard.getPrimaryClip();
            if (d == null || d.getItemCount() == 0) return;
            CharSequence t = d.getItemAt(0).coerceToText(this);
            text = t == null ? "" : t.toString();
        } catch (RuntimeException e) {
            return; // not readable now (in the background, Android 10+)
        }
        if (text.isEmpty() || text.equals(lastClip)) return;
        lastClip = text;
        Log.i(Keeper.TAG, "clipboard to the PC (" + text.length() + " characters)");
        clipSentAt = System.currentTimeMillis();
        Remote.setClipboard(h, text);
    }

    /** The PC's clipboard changed: the phone's follows. */
    private void fromPc(String text) {
        if (clipboard == null || text.isEmpty() || text.equals(lastClip)) return;
        lastClip = text;
        Log.i(Keeper.TAG, "clipboard from the PC (" + text.length() + " characters)");
        clipboard.setPrimaryClip(ClipData.newPlainText("Test Bed", text));
        // Android 13+ shows its own "Copied" message.
        if (!Keeper.isAtLeast(33)) notice("Copied from " + pcName);
    }

    /** Ctrl+{@code code} on the PC. */
    private void ctrl(int code) {
        long h = session;
        if (h == 0) return;
        Remote.key(h, PcKeys.LEFTCTRL, true);
        Remote.key(h, code, true);
        Remote.key(h, code, false);
        Remote.key(h, PcKeys.LEFTCTRL, false);
    }

    /** Pastes the phone's clipboard on the PC: sends it first if it is new. */
    private void paste() {
        sendClipboard();
        // Text sent just now travels beside the keys: give it a moment to land.
        long wait = Math.max(0, 200 - (System.currentTimeMillis() - clipSentAt));
        main.postDelayed(() -> ctrl(PcKeys.KEY_V), wait);
    }

    // ---- keyboard: the PC's keys -------------------------------------------------------------

    /** Presses {@code code} with the latched modifiers (and Shift if asked), then lets go of all. */
    void type(int code, boolean shift) {
        long h = session;
        if (h == 0 || code == 0) return;
        Set<Integer> mods = new HashSet<>(latched);
        if (shift) mods.add(PcKeys.LEFTSHIFT);
        for (int m : mods) Remote.key(h, m, true);
        Remote.key(h, code, true);
        Remote.key(h, code, false);
        for (int m : mods) Remote.key(h, m, false);
        if (!latched.isEmpty()) {
            latched.clear();
            main.post(this::renderMods);
        }
    }

    void typeText(CharSequence text) {
        for (int i = 0; i < text.length(); i++) {
            int[] k = PcKeys.forChar(text.charAt(i));
            if (k == null) {
                if (!warnedChar) {
                    warnedChar = true;
                    main.post(() -> Toast.makeText(this, "Only characters on a US keyboard can be typed on the PC",
                            Toast.LENGTH_SHORT).show());
                }
                continue;
            }
            type(k[0], k[1] == 1);
        }
    }

    private HorizontalScrollView makeStrip() {
        LinearLayout row = new LinearLayout(this);
        row.setPadding(dp(4), dp(2), dp(4), dp(2));
        row.setBackgroundColor(0xEE202020);
        for (Object[] k : new Object[][] {{"Copy", (Runnable) () -> ctrl(PcKeys.KEY_C)}, {"Paste", (Runnable) this::paste}}) {
            Button b = stripButton((String) k[0]);
            Runnable r = (Runnable) k[1];
            b.setOnClickListener(v -> r.run());
            b.setBackground(rounded(0xFF1565C0, dp(6)));
            row.addView(b, stripParams());
        }
        Object[][] keys = {
            {"Esc", PcKeys.ESC}, {"Tab", PcKeys.TAB}, {"Ctrl", -PcKeys.LEFTCTRL}, {"Alt", -PcKeys.LEFTALT},
            {"Shift", -PcKeys.LEFTSHIFT}, {"Win", -PcKeys.LEFTMETA}, {"←", PcKeys.LEFT}, {"↑", PcKeys.UP},
            {"↓", PcKeys.DOWN}, {"→", PcKeys.RIGHT}, {"Home", PcKeys.HOME}, {"End", PcKeys.END},
            {"PgUp", PcKeys.PAGEUP}, {"PgDn", PcKeys.PAGEDOWN}, {"Del", PcKeys.DELETE},
        };
        for (Object[] k : keys) {
            String label = (String) k[0];
            int code = (Integer) k[1];
            Button b = stripButton(label);
            if (code < 0) {
                // Modifiers stay down for the next key.
                int mod = -code;
                modButtons.put(mod, b);
                b.setOnClickListener(v -> {
                    if (!latched.remove(mod)) latched.add(mod);
                    renderMods();
                });
            } else {
                b.setOnClickListener(v -> type(code, false));
            }
            row.addView(b, stripParams());
        }
        HorizontalScrollView s = new HorizontalScrollView(this);
        s.setHorizontalScrollBarEnabled(false);
        s.addView(row);
        return s;
    }

    private Button stripButton(String label) {
        Button b = new Button(this);
        b.setText(label);
        b.setAllCaps(false);
        b.setTextColor(Color.WHITE);
        b.setTextSize(13);
        b.setMinWidth(0);
        b.setMinimumWidth(dp(44));
        b.setMinHeight(0);
        b.setMinimumHeight(dp(36));
        b.setPadding(dp(8), 0, dp(8), 0);
        b.setBackground(rounded(0xFF3A3A3A, dp(6)));
        return b;
    }

    private LinearLayout.LayoutParams stripParams() {
        LinearLayout.LayoutParams lp = new LinearLayout.LayoutParams(LinearLayout.LayoutParams.WRAP_CONTENT, dp(36));
        lp.setMarginEnd(dp(4));
        return lp;
    }

    private void renderMods() {
        for (java.util.Map.Entry<Integer, Button> e : modButtons.entrySet()) {
            e.getValue().setBackground(rounded(latched.contains(e.getKey()) ? 0xFF2E7D32 : 0xFF3A3A3A, dp(6)));
        }
    }

    /** Puts the PC keys just above the phone's keyboard while it is open. */
    private void placeStrip() {
        int ime = imeHeight;
        boolean open = ime > dp(120);
        FrameLayout.LayoutParams lp = (FrameLayout.LayoutParams) strip.getLayoutParams();
        int margin = open ? ime : 0;
        if (open != (strip.getVisibility() == View.VISIBLE) || lp.bottomMargin != margin) {
            lp.bottomMargin = margin;
            strip.setLayoutParams(lp);
            strip.setVisibility(open ? View.VISIBLE : View.GONE);
            if (!open && !latched.isEmpty()) {
                latched.clear();
                renderMods();
            }
        }
    }

    /** An invisible editor: what the keyboard types goes to the PC, nothing stays here. */
    private static final class Typing extends View {
        Typing(Context c) {
            super(c);
            setFocusable(true);
            setFocusableInTouchMode(true);
        }

        @SuppressWarnings("deprecation")
        void show() {
            requestFocus();
            post(() -> {
                InputMethodManager imm = getContext().getSystemService(InputMethodManager.class);
                if (imm == null) return;
                imm.restartInput(this);
                if (!imm.showSoftInput(this, 0)) imm.toggleSoftInput(InputMethodManager.SHOW_FORCED, 0);
            });
        }

        RemoteActivity activity() {
            return (RemoteActivity) getContext();
        }

        /** Keys that skip the keyboard's text path (a hardware keyboard). */
        @Override
        public boolean onKeyDown(int code, KeyEvent e) {
            if (code == KeyEvent.KEYCODE_BACK) return super.onKeyDown(code, e);
            ToPc.send(activity(), e);
            return true;
        }

        @Override
        public boolean onCheckIsTextEditor() {
            return true;
        }

        @Override
        public InputConnection onCreateInputConnection(EditorInfo out) {
            // No suggestions: keyboards then send each letter as it is typed.
            out.inputType = InputType.TYPE_CLASS_TEXT | InputType.TYPE_TEXT_VARIATION_VISIBLE_PASSWORD
                    | InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS;
            out.imeOptions = EditorInfo.IME_FLAG_NO_EXTRACT_UI | EditorInfo.IME_FLAG_NO_FULLSCREEN | EditorInfo.IME_ACTION_NONE;
            return new ToPc(this, activity());
        }
    }

    /** Turns the keyboard's edits into key presses on the PC. */
    private static final class ToPc extends BaseInputConnection {
        private final RemoteActivity a;
        /** Text the keyboard is still composing; the PC already has it typed. */
        private String composing = "";

        ToPc(View v, RemoteActivity a) {
            super(v, false);
            this.a = a;
        }

        /** Types the difference between what the PC has and {@code now}. */
        private void retype(String now) {
            int same = 0;
            while (same < composing.length() && same < now.length() && composing.charAt(same) == now.charAt(same)) same++;
            for (int i = same; i < composing.length(); i++) a.type(PcKeys.BACKSPACE, false);
            a.typeText(now.substring(same));
            composing = now;
        }

        @Override
        public boolean setComposingText(CharSequence text, int newCursorPosition) {
            retype(text.toString());
            return true;
        }

        @Override
        public boolean finishComposingText() {
            composing = "";
            return true;
        }

        @Override
        public boolean commitText(CharSequence text, int newCursorPosition) {
            retype(text.toString());
            composing = "";
            return true;
        }

        @Override
        public boolean deleteSurroundingText(int before, int after) {
            for (int i = 0; i < before; i++) a.type(PcKeys.BACKSPACE, false);
            for (int i = 0; i < after; i++) a.type(PcKeys.DELETE, false);
            return true;
        }

        @Override
        public boolean performEditorAction(int action) {
            a.type(PcKeys.ENTER, false);
            return true;
        }

        @Override
        public boolean sendKeyEvent(KeyEvent e) {
            if (e.getAction() == KeyEvent.ACTION_DOWN) {
                composing = "";
                send(a, e);
            }
            return true;
        }

        static void send(RemoteActivity a, KeyEvent e) {
            int code = PcKeys.forKeyCode(e.getKeyCode());
            if (code != 0) {
                a.type(code, e.isShiftPressed());
                return;
            }
            int c = e.getUnicodeChar();
            if (c != 0) a.typeText(new String(Character.toChars(c)));
        }
    }

    // ---- video and sound ---------------------------------------------------------------------

    /** H.264 to the SurfaceView. Runs on the session thread; the surface comes and goes. */
    private static final class Decoder {
        private final Object lock = new Object();
        private Surface surface;
        private boolean surfaceChanged;
        private int w, h;
        private MediaCodec codec;
        private Thread drain;
        private boolean needKey = true;

        void setSurface(Surface s) {
            synchronized (lock) {
                surface = s;
                surfaceChanged = true;
            }
        }

        void size(int width, int height) {
            synchronized (lock) {
                if (width != w || height != h) surfaceChanged = true;
                w = width;
                h = height;
            }
        }

        /** {@code ev} is the event: 2 header bytes, then the access unit. */
        void frame(byte[] ev, boolean key, long session) {
            Surface s;
            synchronized (lock) {
                s = surface;
                if (surfaceChanged) {
                    surfaceChanged = false;
                    close();
                }
            }
            if (s == null || w <= 0 || h <= 0) return;
            if (codec == null) {
                try {
                    MediaFormat f = MediaFormat.createVideoFormat(MediaFormat.MIMETYPE_VIDEO_AVC, w, h);
                    MediaCodec c = MediaCodec.createDecoderByType(MediaFormat.MIMETYPE_VIDEO_AVC);
                    if (Keeper.isAtLeast(30) && lowLatency(c)) f.setInteger(MediaFormat.KEY_LOW_LATENCY, 1);
                    c.configure(f, s, null, 0);
                    c.start();
                    codec = c;
                    drain = startDrain(c);
                    needKey = true;
                    Remote.keyframe(session);
                } catch (Exception e) {
                    Log.w(Keeper.TAG, "remote decoder: " + e);
                    return;
                }
            }
            if (needKey && !key) return;
            needKey = false;
            try {
                int i = codec.dequeueInputBuffer(20_000);
                ByteBuffer b = i >= 0 ? codec.getInputBuffer(i) : null;
                if (b == null) {
                    // A dropped frame breaks the ones after it until a keyframe.
                    needKey = true;
                    Remote.keyframe(session);
                    return;
                }
                b.clear();
                b.put(ev, 2, ev.length - 2);
                codec.queueInputBuffer(i, 0, ev.length - 2, System.nanoTime() / 1000, 0);
            } catch (IllegalStateException e) {
                Log.d(Keeper.TAG, "remote decoder: " + e);
                close();
            }
        }

        private static boolean lowLatency(MediaCodec c) {
            try {
                return c.getCodecInfo().getCapabilitiesForType(MediaFormat.MIMETYPE_VIDEO_AVC)
                        .isFeatureSupported(android.media.MediaCodecInfo.CodecCapabilities.FEATURE_LowLatency);
            } catch (RuntimeException e) {
                return false;
            }
        }

        private static Thread startDrain(MediaCodec c) {
            Thread t = new Thread(() -> {
                MediaCodec.BufferInfo info = new MediaCodec.BufferInfo();
                try {
                    while (!Thread.currentThread().isInterrupted()) {
                        int i = c.dequeueOutputBuffer(info, 50_000);
                        if (i >= 0) c.releaseOutputBuffer(i, true);
                    }
                } catch (IllegalStateException e) {
                    Log.d(Keeper.TAG, "remote decoder output ended: " + e);
                }
            }, "remote-drain");
            t.start();
            return t;
        }

        private void close() {
            if (drain != null) drain.interrupt();
            if (codec != null) {
                try {
                    if (drain != null) drain.join(500);
                    codec.stop();
                } catch (IllegalStateException e) {
                    Log.d(Keeper.TAG, "stopping remote decoder: " + e);
                } catch (InterruptedException e) {
                    Thread.currentThread().interrupt();
                }
                codec.release();
            }
            codec = null;
            drain = null;
        }

        void release() {
            synchronized (lock) {
                close();
            }
        }
    }

    /** The PC's sound: Opus (48 kHz stereo) through Android's decoder to an AudioTrack. */
    private static final class AudioOut {
        private static final int RATE = 48_000;
        private MediaCodec codec;
        private AudioTrack track;
        private Thread drain;
        private boolean failed;

        void packet(byte[] ev) {
            if (failed) return;
            if (codec == null && !open()) return;
            try {
                int i = codec.dequeueInputBuffer(0);
                if (i < 0) return; // behind: skip a packet rather than lag
                ByteBuffer b = codec.getInputBuffer(i);
                if (b == null) return;
                b.clear();
                b.put(ev, 1, ev.length - 1);
                codec.queueInputBuffer(i, 0, ev.length - 1, System.nanoTime() / 1000, 0);
            } catch (IllegalStateException e) {
                Log.d(Keeper.TAG, "remote audio: " + e);
                release();
                failed = true;
            }
        }

        private boolean open() {
            try {
                MediaFormat f = MediaFormat.createAudioFormat(MediaFormat.MIMETYPE_AUDIO_OPUS, RATE, 2);
                ByteBuffer head = ByteBuffer.allocate(19).order(ByteOrder.LITTLE_ENDIAN);
                head.put("OpusHead".getBytes(java.nio.charset.StandardCharsets.US_ASCII));
                head.put((byte) 1).put((byte) 2).putShort((short) 0).putInt(RATE).putShort((short) 0).put((byte) 0);
                head.flip();
                f.setByteBuffer("csd-0", head);
                f.setByteBuffer("csd-1", nanos(0));
                f.setByteBuffer("csd-2", nanos(80_000_000L));
                MediaCodec c = MediaCodec.createDecoderByType(MediaFormat.MIMETYPE_AUDIO_OPUS);
                c.configure(f, null, null, 0);
                c.start();
                int min = AudioTrack.getMinBufferSize(RATE, AudioFormat.CHANNEL_OUT_STEREO, AudioFormat.ENCODING_PCM_16BIT);
                AudioTrack t = new AudioTrack.Builder()
                        .setAudioAttributes(new AudioAttributes.Builder().setUsage(AudioAttributes.USAGE_MEDIA)
                                .setContentType(AudioAttributes.CONTENT_TYPE_MUSIC).build())
                        .setAudioFormat(new AudioFormat.Builder().setSampleRate(RATE)
                                .setChannelMask(AudioFormat.CHANNEL_OUT_STEREO).setEncoding(AudioFormat.ENCODING_PCM_16BIT).build())
                        .setBufferSizeInBytes(Math.max(min, RATE / 10 * 4))
                        .setTransferMode(AudioTrack.MODE_STREAM)
                        .build();
                t.play();
                codec = c;
                track = t;
                drain = new Thread(this::drain, "remote-audio");
                drain.start();
                return true;
            } catch (Exception e) {
                Log.w(Keeper.TAG, "remote audio unavailable: " + e);
                failed = true;
                return false;
            }
        }

        private static ByteBuffer nanos(long v) {
            ByteBuffer b = ByteBuffer.allocate(8).order(ByteOrder.nativeOrder());
            b.putLong(v);
            b.flip();
            return b;
        }

        private void drain() {
            MediaCodec c = codec;
            AudioTrack t = track;
            MediaCodec.BufferInfo info = new MediaCodec.BufferInfo();
            byte[] pcm = new byte[0];
            try {
                while (!Thread.currentThread().isInterrupted()) {
                    int i = c.dequeueOutputBuffer(info, 50_000);
                    if (i < 0) continue;
                    ByteBuffer b = c.getOutputBuffer(i);
                    if (b != null && info.size > 0) {
                        if (pcm.length < info.size) pcm = new byte[info.size];
                        b.position(info.offset);
                        b.get(pcm, 0, info.size);
                        t.write(pcm, 0, info.size);
                    }
                    c.releaseOutputBuffer(i, false);
                }
            } catch (IllegalStateException e) {
                Log.d(Keeper.TAG, "remote audio output ended: " + e);
            }
        }

        void release() {
            if (drain != null) {
                drain.interrupt();
                try {
                    drain.join(500);
                } catch (InterruptedException e) {
                    Thread.currentThread().interrupt();
                }
            }
            if (codec != null) {
                try {
                    codec.stop();
                } catch (IllegalStateException ignored) {
                    // already stopped
                }
                codec.release();
            }
            if (track != null) track.release();
            codec = null;
            track = null;
            drain = null;
        }
    }

    // ---- helpers -----------------------------------------------------------------------------

    private static GradientDrawable rounded(int color, int radius) {
        GradientDrawable d = new GradientDrawable();
        d.setColor(color);
        d.setCornerRadius(radius);
        return d;
    }

    private int dp(int v) {
        return Math.round(v * getResources().getDisplayMetrics().density);
    }

    static void open(Context c, String fingerprint, String name) {
        c.startActivity(new Intent(c, RemoteActivity.class).putExtra(EXTRA_FINGERPRINT, fingerprint).putExtra(EXTRA_NAME, name));
    }
}
