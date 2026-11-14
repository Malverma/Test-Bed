package org.androidcontroller.keeper;

import android.view.KeyEvent;

/**
 * Phone keyboard input as Linux evdev key codes, which Test Bed sends to the
 * PC (it presses the same keys there, so the PC's own layout applies; this
 * table is the US layout).
 */
final class PcKeys {
    static final int ESC = 1, BACKSPACE = 14, TAB = 15, ENTER = 28, LEFTCTRL = 29, LEFTSHIFT = 42, LEFTALT = 56, SPACE = 57,
            HOME = 102, UP = 103, PAGEUP = 104, LEFT = 105, RIGHT = 106, END = 107, DOWN = 108, PAGEDOWN = 109, INSERT = 110,
            DELETE = 111, LEFTMETA = 125, F1 = 59, KEY_C = 46, KEY_V = 47;

    static final int BTN_LEFT = 0x110, BTN_RIGHT = 0x111, BTN_MIDDLE = 0x112;

    private static final String ROW_Q = "qwertyuiop", ROW_A = "asdfghjkl", ROW_Z = "zxcvbnm";
    private static final String PLAIN = "1234567890-=[];'`\\,./";
    private static final String SHIFTED = "!@#$%^&*()_+{}:\"~|<>?";
    private static final int[] PLAIN_CODES = {2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 26, 27, 39, 40, 41, 43, 51, 52, 53};

    private PcKeys() {}

    /** {code, shift (1 or 0)} for a typed character, or null if it has no key. */
    static int[] forChar(char c) {
        char l = Character.toLowerCase(c);
        boolean upper = c != l;
        int i;
        if ((i = ROW_Q.indexOf(l)) >= 0) return new int[] {16 + i, upper ? 1 : 0};
        if ((i = ROW_A.indexOf(l)) >= 0) return new int[] {30 + i, upper ? 1 : 0};
        if ((i = ROW_Z.indexOf(l)) >= 0) return new int[] {44 + i, upper ? 1 : 0};
        if ((i = PLAIN.indexOf(c)) >= 0) return new int[] {PLAIN_CODES[i], 0};
        if ((i = SHIFTED.indexOf(c)) >= 0) return new int[] {PLAIN_CODES[i], 1};
        switch (c) {
            case ' ': return new int[] {SPACE, 0};
            case '\n': return new int[] {ENTER, 0};
            case '\t': return new int[] {TAB, 0};
            default: return null;
        }
    }

    /** Android key codes for keys that aren't characters. */
    static int forKeyCode(int code) {
        switch (code) {
            case KeyEvent.KEYCODE_DEL: return BACKSPACE;
            case KeyEvent.KEYCODE_FORWARD_DEL: return DELETE;
            case KeyEvent.KEYCODE_ENTER:
            case KeyEvent.KEYCODE_NUMPAD_ENTER: return ENTER;
            case KeyEvent.KEYCODE_TAB: return TAB;
            case KeyEvent.KEYCODE_ESCAPE: return ESC;
            case KeyEvent.KEYCODE_DPAD_LEFT: return LEFT;
            case KeyEvent.KEYCODE_DPAD_RIGHT: return RIGHT;
            case KeyEvent.KEYCODE_DPAD_UP: return UP;
            case KeyEvent.KEYCODE_DPAD_DOWN: return DOWN;
            case KeyEvent.KEYCODE_MOVE_HOME: return HOME;
            case KeyEvent.KEYCODE_MOVE_END: return END;
            case KeyEvent.KEYCODE_PAGE_UP: return PAGEUP;
            case KeyEvent.KEYCODE_PAGE_DOWN: return PAGEDOWN;
            case KeyEvent.KEYCODE_INSERT: return INSERT;
            default:
                if (code >= KeyEvent.KEYCODE_F1 && code <= KeyEvent.KEYCODE_F10) return F1 + code - KeyEvent.KEYCODE_F1;
                if (code == KeyEvent.KEYCODE_F11) return 87;
                if (code == KeyEvent.KEYCODE_F12) return 88;
                return 0;
        }
    }
}
