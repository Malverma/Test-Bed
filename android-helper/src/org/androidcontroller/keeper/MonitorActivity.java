package org.androidcontroller.keeper;

import android.app.Activity;
import android.content.pm.ActivityInfo;
import android.graphics.Color;
import android.os.Bundle;
import android.os.Handler;
import android.os.Looper;
import android.content.Context;
import android.text.InputType;
import android.view.Gravity;
import android.view.KeyEvent;
import android.view.MotionEvent;
import android.view.SurfaceHolder;
import android.view.SurfaceView;
import android.view.View;
import android.view.Window;
import android.view.WindowManager;
import android.view.inputmethod.BaseInputConnection;
import android.view.inputmethod.EditorInfo;
import android.view.inputmethod.InputConnection;
import android.view.inputmethod.InputMethodManager;
import android.widget.Button;
import android.widget.FrameLayout;
import android.widget.TextView;
import android.widget.Toast;

import org.json.JSONArray;
import org.json.JSONException;
import org.json.JSONObject;

/**
 * The full screen picture of the PC. Touches go to the PC's pointer; a swipe
 * up from the bottom edge opens the keyboard, which types on the PC.
 */
public final class MonitorActivity extends Activity {
    private static final long EXIT_BAR_MS = 5_000;
    private static final int EXIT_FINGERS = 3;
    private static final long MIN_MOVE_GAP_MS = 8;
    /** A swipe starting this close to the bottom (fraction of the height) may open the keyboard. */
    private static final float EDGE = 0.07f;
    /** …once it has moved up this far. */
    private static final float SWIPE = 0.12f;

    private final Handler handler = new Handler(Looper.getMainLooper());
    private TextView statusView;
    private Button exitBar;
    private final Runnable hideExitBar = () -> exitBar.setVisibility(View.GONE);
    /** Three fingers are down or were: this gesture is not for the PC. */
    private boolean gestureIsOurs;
    private long lastMoveSent;
    private FrameLayout root;
    private SurfaceView video;
    private Typing typing;
    /** A one-finger gesture that began at the bottom edge, held back from the PC. */
    private MotionEvent edgeDown;

    @Override
    protected void onCreate(Bundle saved) {
        super.onCreate(saved);
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
        setRequestedOrientation(Monitor.portrait() ? ActivityInfo.SCREEN_ORIENTATION_SENSOR_PORTRAIT
                : ActivityInfo.SCREEN_ORIENTATION_SENSOR_LANDSCAPE);

        root = new FrameLayout(this);
        root.setBackgroundColor(Color.BLACK);
        root.addOnLayoutChangeListener((v, l, t, r, b, ol, ot, or, ob) -> {
            if (r - l != or - ol || b - t != ob - ot) v.post(this::onVideoSize);
        });

        typing = new Typing(this);
        root.addView(typing, new FrameLayout.LayoutParams(1, 1));

        video = new SurfaceView(this);
        video.getHolder().addCallback(new SurfaceHolder.Callback() {
            @Override
            public void surfaceCreated(SurfaceHolder h) {
                Monitor.setSurface(h.getSurface());
            }

            @Override
            public void surfaceChanged(SurfaceHolder h, int format, int width, int height) {
                Monitor.setSurface(h.getSurface());
            }

            @Override
            public void surfaceDestroyed(SurfaceHolder h) {
                Monitor.setSurface(null);
            }
        });
        root.addView(video, new FrameLayout.LayoutParams(FrameLayout.LayoutParams.MATCH_PARENT, FrameLayout.LayoutParams.MATCH_PARENT, Gravity.CENTER));

        statusView = new TextView(this);
        statusView.setTextColor(Color.WHITE);
        statusView.setTextSize(20);
        statusView.setGravity(Gravity.CENTER);
        root.addView(statusView, new FrameLayout.LayoutParams(FrameLayout.LayoutParams.WRAP_CONTENT, FrameLayout.LayoutParams.WRAP_CONTENT, Gravity.CENTER));

        exitBar = new Button(this);
        exitBar.setText("Exit Monitor");
        exitBar.setVisibility(View.GONE);
        exitBar.setOnClickListener(v -> Monitor.userToggle(this, false));
        root.addView(exitBar, new FrameLayout.LayoutParams(FrameLayout.LayoutParams.WRAP_CONTENT, FrameLayout.LayoutParams.WRAP_CONTENT, Gravity.BOTTOM | Gravity.CENTER_HORIZONTAL));

        setContentView(root);
    }

    @Override
    protected void onResume() {
        super.onResume();
        if (!Monitor.active() && !Monitor.pcConnected()) {
            finish(); // reopened from recents after the session ended
            return;
        }
        hideSystemBars();
        Monitor.attach(this);
        root.post(this::onVideoSize);
    }

    @Override
    protected void onPause() {
        Monitor.detach(this);
        super.onPause();
    }

    @Override
    public void onWindowFocusChanged(boolean hasFocus) {
        super.onWindowFocusChanged(hasFocus);
        if (hasFocus) hideSystemBars();
    }

    @SuppressWarnings("deprecation")
    private void hideSystemBars() {
        getWindow().getDecorView().setSystemUiVisibility(View.SYSTEM_UI_FLAG_IMMERSIVE_STICKY
                | View.SYSTEM_UI_FLAG_HIDE_NAVIGATION | View.SYSTEM_UI_FLAG_FULLSCREEN
                | View.SYSTEM_UI_FLAG_LAYOUT_STABLE | View.SYSTEM_UI_FLAG_LAYOUT_HIDE_NAVIGATION
                | View.SYSTEM_UI_FLAG_LAYOUT_FULLSCREEN);
    }

    /** Keeps the picture's shape (a mirrored PC screen may not match the phone's). */
    void onVideoSize() {
        int vw = Monitor.videoWidth(), vh = Monitor.videoHeight();
        int rw = root.getWidth(), rh = root.getHeight();
        if (vw <= 0 || vh <= 0 || rw <= 0 || rh <= 0) return;
        float scale = Math.min((float) rw / vw, (float) rh / vh);
        FrameLayout.LayoutParams lp = (FrameLayout.LayoutParams) video.getLayoutParams();
        int w = Math.round(vw * scale), h = Math.round(vh * scale);
        if (lp.width == w && lp.height == h) return;
        lp.width = w;
        lp.height = h;
        video.setLayoutParams(lp);
    }

    void onStatus(String text) {
        statusView.setText(text);
        statusView.setVisibility(text.isEmpty() ? View.GONE : View.VISIBLE);
    }

    @Override
    public void onBackPressed() {
        // A stray swipe must not close the display.
        Toast.makeText(this, "Tap with three fingers to exit", Toast.LENGTH_SHORT).show();
    }

    @Override
    public boolean onTouchEvent(MotionEvent e) {
        int action = e.getActionMasked();
        if (action == MotionEvent.ACTION_DOWN) {
            gestureIsOurs = false;
            dropEdge();
            View d = getWindow().getDecorView();
            if (!Monitor.viewOnly() && e.getY() > d.getHeight() * (1 - EDGE)) edgeDown = MotionEvent.obtain(e);
        }
        if (edgeDown != null && !gestureIsOurs) {
            if (e.getPointerCount() > 1 || action == MotionEvent.ACTION_CANCEL) {
                dropEdge();
                gestureIsOurs = true; // not a tap, not a swipe: leave the PC alone
            } else if (action == MotionEvent.ACTION_MOVE) {
                if (edgeDown.getY() - e.getY() > getWindow().getDecorView().getHeight() * SWIPE) {
                    dropEdge();
                    gestureIsOurs = true;
                    typing.show();
                }
                return true;
            } else if (action == MotionEvent.ACTION_UP) {
                // Just a tap near the edge: the PC gets it after all.
                sendTouch("down", edgeDown);
                sendTouch("up", e);
                dropEdge();
                return true;
            } else {
                return true;
            }
        }
        if (!gestureIsOurs && e.getPointerCount() >= EXIT_FINGERS) {
            gestureIsOurs = true;
            sendTouch("cancel", e);
            exitBar.setVisibility(View.VISIBLE);
            handler.removeCallbacks(hideExitBar);
            handler.postDelayed(hideExitBar, EXIT_BAR_MS);
            return true;
        }
        if (gestureIsOurs || Monitor.viewOnly()) return true;
        switch (action) {
            case MotionEvent.ACTION_DOWN:
            case MotionEvent.ACTION_POINTER_DOWN:
                sendTouch("down", e);
                break;
            case MotionEvent.ACTION_MOVE:
                long now = System.currentTimeMillis();
                if (now - lastMoveSent >= MIN_MOVE_GAP_MS) {
                    lastMoveSent = now;
                    sendTouch("move", e);
                }
                break;
            case MotionEvent.ACTION_UP:
                sendTouch("up", e);
                break;
            case MotionEvent.ACTION_POINTER_UP:
                sendTouch("pointer_up", e);
                break;
            case MotionEvent.ACTION_CANCEL:
                sendTouch("cancel", e);
                break;
            default:
                break;
        }
        return true;
    }

    private void dropEdge() {
        if (edgeDown != null) edgeDown.recycle();
        edgeDown = null;
    }

    /** Positions are 0..1 of the picture, so the PC needs no phone sizes. */
    private void sendTouch(String action, MotionEvent e) {
        int[] at = new int[2];
        video.getLocationInWindow(at);
        float w = Math.max(1, video.getWidth()), h = Math.max(1, video.getHeight());
        try {
            JSONArray points = new JSONArray();
            for (int i = 0; i < e.getPointerCount(); i++) {
                JSONArray p = new JSONArray();
                p.put(e.getPointerId(i));
                p.put((double) clamp((e.getX(i) - at[0]) / w));
                p.put((double) clamp((e.getY(i) - at[1]) / h));
                points.put(p);
            }
            JSONObject o = Proto.obj("op", "touch", "a", action, "t", e.getEventTime(), "p", points);
            Monitor.sendTouch(o);
        } catch (JSONException ex) {
            throw new IllegalStateException(ex);
        }
    }

    private static float clamp(float v) {
        return Math.max(0f, Math.min(1f, v));
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
            // The keyboard only answers once this view is the one it serves.
            post(() -> {
                InputMethodManager imm = getContext().getSystemService(InputMethodManager.class);
                if (imm == null) return;
                imm.restartInput(this);
                if (!imm.showSoftInput(this, 0)) imm.toggleSoftInput(InputMethodManager.SHOW_FORCED, 0);
            });
        }

        /** Keys that skip the keyboard's text path (a hardware keyboard, or adb). */
        @Override
        public boolean onKeyDown(int code, KeyEvent e) {
            if (code == KeyEvent.KEYCODE_BACK) return super.onKeyDown(code, e);
            ToPc.send(e);
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
            return new ToPc(this);
        }
    }

    /** Turns the keyboard's edits into text and keys for the PC. */
    private static final class ToPc extends BaseInputConnection {
        /** Text the keyboard is still composing; the PC already has it typed. */
        private String composing = "";

        ToPc(View v) {
            super(v, false);
        }

        /** Types the difference between what the PC has and {@code now}. */
        private void retype(String now) {
            int same = 0;
            while (same < composing.length() && same < now.length() && composing.charAt(same) == now.charAt(same)) same++;
            for (int i = same; i < composing.length(); i++) Monitor.sendKey("BackSpace");
            Monitor.sendText(now.substring(same));
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
            for (int i = 0; i < before; i++) Monitor.sendKey("BackSpace");
            for (int i = 0; i < after; i++) Monitor.sendKey("Delete");
            return true;
        }

        @Override
        public boolean performEditorAction(int action) {
            Monitor.sendKey("Return");
            return true;
        }

        @Override
        public boolean sendKeyEvent(KeyEvent e) {
            if (e.getAction() == KeyEvent.ACTION_DOWN) {
                composing = "";
                send(e);
            }
            return true;
        }

        static void send(KeyEvent e) {
            String key = keyName(e.getKeyCode());
            if (key != null) {
                Monitor.sendKey(key);
            } else {
                int c = e.getUnicodeChar();
                if (c != 0) Monitor.sendText(new String(Character.toChars(c)));
            }
        }

        private static String keyName(int code) {
            switch (code) {
                case KeyEvent.KEYCODE_DEL: return "BackSpace";
                case KeyEvent.KEYCODE_FORWARD_DEL: return "Delete";
                case KeyEvent.KEYCODE_ENTER:
                case KeyEvent.KEYCODE_NUMPAD_ENTER: return "Return";
                case KeyEvent.KEYCODE_TAB: return "Tab";
                case KeyEvent.KEYCODE_ESCAPE: return "Escape";
                case KeyEvent.KEYCODE_DPAD_LEFT: return "Left";
                case KeyEvent.KEYCODE_DPAD_RIGHT: return "Right";
                case KeyEvent.KEYCODE_DPAD_UP: return "Up";
                case KeyEvent.KEYCODE_DPAD_DOWN: return "Down";
                case KeyEvent.KEYCODE_MOVE_HOME: return "Home";
                case KeyEvent.KEYCODE_MOVE_END: return "End";
                default: return null;
            }
        }
    }
}
