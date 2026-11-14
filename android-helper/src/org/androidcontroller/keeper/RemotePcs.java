package org.androidcontroller.keeper;

import android.app.Activity;
import android.app.AlertDialog;
import android.net.wifi.WifiManager;
import android.os.Handler;
import android.os.Looper;
import android.text.InputType;
import android.view.Gravity;
import android.view.View;
import android.widget.Button;
import android.widget.EditText;
import android.widget.LinearLayout;
import android.widget.TextView;
import android.widget.Toast;

import org.json.JSONArray;
import org.json.JSONObject;

import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;

/**
 * The main screen's "Remote PCs": the PCs this phone may control, whether
 * each answers, and pairing new ones, on this network with the PIN the PC
 * shows or from another network with its invite code.
 */
final class RemotePcs {
    private static final int TEXT = 0xFFECECEC;
    private static final int DIM = 0xFF9E9E9E;
    private static final int GOOD = 0xFF81C784;
    private static final long PROBE_EVERY_MS = 10_000;

    private final Activity a;
    private final LinearLayout list;
    private final TextView empty;
    private final ExecutorService work = Executors.newSingleThreadExecutor();
    private final Handler main = new Handler(Looper.getMainLooper());
    private final boolean available;
    private JSONArray hosts = new JSONArray();
    private JSONObject online = new JSONObject();
    private boolean resumed;
    private final Runnable probeLoop = new Runnable() {
        @Override
        public void run() {
            refresh(true);
            main.postDelayed(this, PROBE_EVERY_MS);
        }
    };

    RemotePcs(Activity a, LinearLayout card) {
        this.a = a;
        available = Remote.available(a);
        list = new LinearLayout(a);
        list.setOrientation(LinearLayout.VERTICAL);
        card.addView(list);
        empty = text(available ? "No PCs yet. Turn on Share this PC in Test Bed's PC tab on the PC, then add it here."
                : "Needs Android 7 or newer.", 13, DIM);
        card.addView(empty);
        if (!available) return;
        LinearLayout buttons = new LinearLayout(a);
        buttons.addView(button("Add nearby PC", v -> addLan()), weighted());
        buttons.addView(button("Add with invite code", v -> addInternet()), weighted());
        card.addView(buttons);
    }

    void resume() {
        if (!available) return;
        resumed = true;
        main.removeCallbacks(probeLoop);
        main.post(probeLoop);
    }

    void pause() {
        resumed = false;
        main.removeCallbacks(probeLoop);
    }

    /** Reloads the list; with {@code probe}, also asks each PC whether it is there. */
    private void refresh(boolean probe) {
        work.execute(() -> {
            try {
                JSONArray h = (JSONArray) Remote.ok(Remote.hosts());
                main.post(() -> {
                    hosts = h;
                    render();
                });
                if (probe && h.length() > 0 && resumed) {
                    JSONObject o = (JSONObject) Remote.ok(Remote.probe());
                    main.post(() -> {
                        online = o;
                        render();
                    });
                }
            } catch (Remote.Failed e) {
                main.post(() -> toast(e.getMessage()));
            }
        });
    }

    private void render() {
        list.removeAllViews();
        empty.setVisibility(hosts.length() == 0 ? View.VISIBLE : View.GONE);
        for (int i = 0; i < hosts.length(); i++) {
            JSONObject h = hosts.optJSONObject(i);
            if (h == null) continue;
            String name = h.optString("name"), fp = h.optString("fingerprint");
            boolean internet = h.optBoolean("internet");
            boolean known = online.has(fp);
            boolean up = known && !online.isNull(fp);

            LinearLayout row = new LinearLayout(a);
            row.setGravity(Gravity.CENTER_VERTICAL);
            row.setPadding(0, dp(4), 0, dp(4));
            LinearLayout labels = new LinearLayout(a);
            labels.setOrientation(LinearLayout.VERTICAL);
            labels.addView(text(name, 15, TEXT));
            String where = internet ? "Another network" : "This network";
            String state = !known ? "" : up ? " · online, " + Math.round(online.optDouble(fp)) + " ms" : " · offline";
            TextView sub = text(where + state, 12, up ? GOOD : DIM);
            labels.addView(sub);
            row.addView(labels, new LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 1));
            row.addView(button("Connect", v -> RemoteActivity.open(a, fp, name)));
            row.setOnLongClickListener(v -> {
                new AlertDialog.Builder(a)
                        .setTitle("Forget " + name + "?")
                        .setMessage("To control it again, pair it again.")
                        .setPositiveButton("Forget", (d, w) -> work.execute(() -> {
                            try {
                                Remote.ok(Remote.forget(fp));
                            } catch (Remote.Failed e) {
                                main.post(() -> toast(e.getMessage()));
                            }
                            refresh(false);
                        }))
                        .setNegativeButton("Cancel", null)
                        .show();
                return true;
            });
            list.addView(row);
        }
    }

    // ---- pairing -----------------------------------------------------------------------------

    /** Looks for PCs sharing on this Wi-Fi, then asks for the PIN the chosen one shows. */
    private void addLan() {
        toast("Looking for PCs on this network…");
        work.execute(() -> {
            WifiManager wifi = a.getApplicationContext().getSystemService(WifiManager.class);
            WifiManager.MulticastLock lock = wifi == null ? null : wifi.createMulticastLock("test-bed-find-pcs");
            JSONArray found;
            try {
                if (lock != null) lock.acquire();
                found = (JSONArray) Remote.ok(Remote.discover());
            } catch (Remote.Failed e) {
                found = new JSONArray();
            } finally {
                if (lock != null && lock.isHeld()) lock.release();
            }
            JSONArray f = found;
            main.post(() -> chooseLan(f));
        });
    }

    private void chooseLan(JSONArray found) {
        List<String> labels = new ArrayList<>();
        List<String> addresses = new ArrayList<>();
        List<String> names = new ArrayList<>();
        for (int i = 0; i < found.length(); i++) {
            JSONObject d = found.optJSONObject(i);
            if (d == null) continue;
            labels.add(d.optString("name") + (d.optBoolean("pairing") ? "  ·  showing a PIN" : ""));
            addresses.add(d.optString("address"));
            names.add(d.optString("name"));
        }
        labels.add("Type its address…");
        new AlertDialog.Builder(a)
                .setTitle(found.length() == 0 ? "No PCs found" : "Add a PC")
                .setItems(labels.toArray(new String[0]), (d, which) -> {
                    if (which < addresses.size()) {
                        askPin(addresses.get(which), names.get(which));
                    } else {
                        askAddress();
                    }
                })
                .setNegativeButton("Cancel", null)
                .show();
    }

    private void askAddress() {
        EditText e = field("192.168.1.20", InputType.TYPE_CLASS_TEXT | InputType.TYPE_TEXT_VARIATION_URI);
        new AlertDialog.Builder(a)
                .setTitle("PC address")
                .setView(padded(e))
                .setPositiveButton("Next", (d, w) -> {
                    String s = e.getText().toString().trim();
                    if (!s.isEmpty()) askPin(s, s);
                })
                .setNegativeButton("Cancel", null)
                .show();
    }

    private void askPin(String address, String name) {
        EditText e = field("6-digit PIN", InputType.TYPE_CLASS_NUMBER);
        new AlertDialog.Builder(a)
                .setTitle("Pair with " + name)
                .setMessage("On the PC, open Test Bed › PC › Pair a computer and type the PIN it shows.")
                .setView(padded(e))
                .setPositiveButton("Pair", (d, w) -> pair(() -> Remote.pairLan(address, e.getText().toString(), Remote.deviceName())))
                .setNegativeButton("Cancel", null)
                .show();
    }

    private void addInternet() {
        EditText e = field("Invite code", InputType.TYPE_CLASS_TEXT | InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS);
        e.setSingleLine(false);
        new AlertDialog.Builder(a)
                .setTitle("Add a PC on another network")
                .setMessage("On the PC, turn on Test Bed › PC › Allow access from other networks, then Invite a computer "
                        + "on another network, and send yourself the code.")
                .setView(padded(e))
                .setPositiveButton("Add", (d, w) -> pair(() -> Remote.pairInternet(e.getText().toString(), Remote.deviceName())))
                .setNegativeButton("Cancel", null)
                .show();
    }

    private interface Call {
        String run();
    }

    private void pair(Call call) {
        toast("Pairing…");
        work.execute(() -> {
            try {
                JSONObject h = (JSONObject) Remote.ok(call.run());
                main.post(() -> toast("Paired with " + h.optString("name")));
            } catch (Remote.Failed e) {
                main.post(() -> toast("Didn't pair: " + e.getMessage()));
            }
            refresh(true);
        });
    }

    // ---- small view helpers ------------------------------------------------------------------

    private void toast(String s) {
        Toast.makeText(a, s, Toast.LENGTH_LONG).show();
    }

    private EditText field(String hint, int type) {
        EditText e = new EditText(a);
        e.setHint(hint);
        e.setInputType(type);
        return e;
    }

    private View padded(View v) {
        LinearLayout l = new LinearLayout(a);
        l.setPadding(dp(20), dp(4), dp(20), 0);
        l.addView(v, new LinearLayout.LayoutParams(LinearLayout.LayoutParams.MATCH_PARENT, LinearLayout.LayoutParams.WRAP_CONTENT));
        return l;
    }

    private TextView text(String s, int sp, int color) {
        TextView t = new TextView(a);
        t.setText(s);
        t.setTextSize(sp);
        t.setTextColor(color);
        return t;
    }

    private Button button(String label, View.OnClickListener onClick) {
        Button b = new Button(a);
        b.setText(label);
        b.setAllCaps(false);
        b.setOnClickListener(onClick);
        return b;
    }

    private static LinearLayout.LayoutParams weighted() {
        return new LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 1);
    }

    private int dp(int v) {
        return Math.round(v * a.getResources().getDisplayMetrics().density);
    }
}
