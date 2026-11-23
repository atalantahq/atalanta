package dev.velocity.echo;

import android.app.Activity;
import android.content.Intent;
import android.os.Bundle;
import android.util.TypedValue;
import android.view.Gravity;
import android.widget.LinearLayout;
import android.widget.ScrollView;
import android.widget.TextView;

import java.util.TreeSet;

public class MainActivity extends Activity {

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        render();
    }

    @Override
    protected void onNewIntent(Intent intent) {
        super.onNewIntent(intent);
        setIntent(intent);
        render();
    }

    private void render() {
        ScrollView scroll = new ScrollView(this);
        LinearLayout layout = new LinearLayout(this);
        layout.setOrientation(LinearLayout.VERTICAL);
        layout.setPadding(32, 32, 32, 32);
        layout.setGravity(Gravity.START);

        android.os.Bundle extras = getIntent().getExtras();

        TextView title = new TextView(this);
        title.setText("Velocity Echo");
        title.setTextSize(TypedValue.COMPLEX_UNIT_SP, 18);
        title.setTextIsSelectable(false);
        layout.addView(title);

        int count = extras == null ? 0 : extras.size();

        TextView countRow = new TextView(this);
        countRow.setText("extras=" + count);
        countRow.setTextSize(TypedValue.COMPLEX_UNIT_SP, 18);
        countRow.setTextIsSelectable(false);
        layout.addView(countRow);

        if (extras == null || extras.size() == 0) {
            TextView none = new TextView(this);
            none.setText("no extras");
            none.setTextSize(TypedValue.COMPLEX_UNIT_SP, 18);
            none.setTextIsSelectable(false);
            layout.addView(none);
        } else {
            TreeSet<String> keys = new TreeSet<>(extras.keySet());
            for (String key : keys) {
                Object value = extras.get(key);
                String type;
                if (value instanceof Boolean) {
                    type = "boolean";
                } else if (value instanceof Integer) {
                    type = "int";
                } else if (value instanceof Long) {
                    type = "long";
                } else if (value instanceof Float) {
                    type = "float";
                } else if (value instanceof Double) {
                    type = "double";
                } else if (value instanceof String) {
                    type = "string";
                } else {
                    type = "other";
                }
                TextView row = new TextView(this);
                row.setText(key + "=" + String.valueOf(value) + " (" + type + ")");
                row.setTextSize(TypedValue.COMPLEX_UNIT_SP, 18);
                row.setTextIsSelectable(false);
                layout.addView(row);
            }
        }

        scroll.addView(layout);
        setContentView(scroll);
    }
}
