package dev.vitral.ime

import android.Manifest
import android.content.ClipboardManager
import android.content.Intent
import android.content.pm.PackageManager
import android.graphics.Color
import android.graphics.drawable.GradientDrawable
import android.inputmethodservice.InputMethodService
import android.os.Build
import android.os.VibrationEffect
import android.os.Vibrator
import android.view.*
import android.view.inputmethod.EditorInfo
import android.widget.*
import dev.vitral.ime.core.*
import dev.vitral.ime.theme.VitralTokens as T
import org.json.JSONArray
import org.json.JSONObject

class VitralIme : InputMethodService(), ClipboardManager.OnPrimaryClipChangedListener {
    private lateinit var frame: FrameLayout
    private lateinit var sheet: LinearLayout
    private lateinit var bar: LinearLayout
    private lateinit var body: LinearLayout
    private val clipboard by lazy { getSystemService(ClipboardManager::class.java) }
    private var editor = 0
    private var canvasGeneration = 0
    private var active = false
    private var floating = false
    private var flypy = true
    private var numbers = false
    private var expanded = false
    private var mode = "keys"
    private var inlineRequest = 0
    private var english = false
    private var composing = false
    private var paste = ""
    private var shown = JSONArray()
    private var font = 16f
    private var floatWidth = 0
    private var floatHeight = 0
    private var offsetX = 0
    private var offsetY = 0
    private var lastClip = -1L
    private val microphone = Microphone()
    private val main = android.os.Handler(android.os.Looper.getMainLooper())

    private var alive = false
    private val pollClipboard =
        object : Runnable {
            override fun run() {
                if (!alive) return
                Core.call(this@VitralIme, "poll-services", background = true) {
                    if (alive) {
                        if (active) call("quick-paste-state") { state -> updatePaste(state) }
                        main.postDelayed(this, 3000)
                    }
                }
            }
        }

    override fun onEvaluateFullscreenMode() = false

    override fun onCreate() {
        super.onCreate()
        alive = true
        main.post(pollClipboard)
        clipboard.addPrimaryClipChangedListener(this)
    }

    override fun onDestroy() {
        alive = false
        main.removeCallbacks(pollClipboard)
        invalidateEditor()
        clipboard.removePrimaryClipChangedListener(this)
        super.onDestroy()
    }

    override fun onCreateInputView(): View {
        frame = FrameLayout(this)
        sheet = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        frame.addView(sheet)
        layoutPanel()
        render()
        return frame
    }

    override fun onStartInputView(info: EditorInfo?, restarting: Boolean) {
        super.onStartInputView(info, restarting)
        invalidateEditor()
        active = true
        mode = "keys"
        expanded = false
        shown = JSONArray()
        paste = ""
        composing = false
        val cfg = Core.settings(this)
        flypy = cfg.optString("schema", "double_pinyin_flypy") == "double_pinyin_flypy"
        font = cfg.optInt("candidate_font_px", 16).coerceIn(13, 22).toFloat()
        Core.call(this, "process-key", json("keycode" to "Escape"))
        call("on-app-focus", json("app_id" to (info?.packageName ?: "android"))) {
            english = it.optString("lang_mode") == "en"
            render()
        }
        call("on-ime-open") { updatePaste(it) }
        layoutPanel()
        render()
    }

    override fun onFinishInputView(finishingInput: Boolean) {
        invalidateEditor()
        Core.call(this, "on-ime-close")
        super.onFinishInputView(finishingInput)
    }

    private fun invalidateEditor() {
        inlineRequest++
        active = false
        editor++
        canvasGeneration++
        microphone.cancel()
    }

    private fun call(
        cmd: String,
        args: JSONObject = JSONObject(),
        done: (JSONObject) -> Unit = {},
    ) {
        val id = editor
        Core.call(this, cmd, args) {
            if (active && editor == id)
                it.onSuccess { v -> done(v as JSONObject) }
                    .onFailure { e -> message(e.message.orEmpty()) }
        }
    }

    private fun message(text: String) {
        Toast.makeText(this, text, Toast.LENGTH_SHORT).show()
    }

    private fun updatePaste(state: JSONObject) {
        val value = state.optJSONObject("quick_paste")?.optString("content").orEmpty()
        if (value != paste) {
            paste = value
            candidates()
        }
    }

    override fun onPrimaryClipChanged() {
        val stamp = clipboard.primaryClipDescription?.timestamp ?: return
        if (lastClip == stamp) return
        val text =
            clipboard.primaryClip?.takeIf { it.itemCount > 0 }?.getItemAt(0)?.text?.toString()
                ?: return
        if (text.isEmpty()) return
        lastClip = stamp
        Core.call(this, "clipboard-push", json("payload" to text), background = true) {
            if (active) call("quick-paste-state") { updatePaste(it) }
        }
    }

    private fun layoutPanel() {
        if (!::frame.isInitialized) return
        val dm = resources.displayMetrics
        val win = window.window ?: return
        val insets = win.decorView.rootWindowInsets
        val nav =
            if (Build.VERSION.SDK_INT >= 30) {
                getSystemService(WindowManager::class.java)
                    .maximumWindowMetrics
                    .windowInsets
                    .getInsetsIgnoringVisibility(WindowInsets.Type.navigationBars())
                    .bottom
            } else {
                @Suppress("DEPRECATION")
                insets?.stableInsetBottom ?: 0
            }
        val usableHeight = (dm.heightPixels - nav).coerceAtLeast(1)
        val dock = (dm.heightPixels * T.Ergonomics.mobileDockedScreenRatio).toInt()
        val bounds =
            panelBounds(
                dm.widthPixels,
                usableHeight,
                if (floating) floatWidth.takeIf { it > 0 } ?: (dm.widthPixels * 3 / 4)
                else dm.widthPixels,
                if (floating && floatHeight > 0) floatHeight else dock,
                if (floating) dp(T.Ergonomics.floatingMinWidthDp) else 0,
                if (floating) dp(T.Ergonomics.floatingMinHeightDp) else 0,
                if (floating) offsetX else 0,
                if (floating) offsetY else 0,
            )
        offsetX = bounds.left
        offsetY = bounds.bottom
        sheet.layoutParams = FrameLayout.LayoutParams(bounds.width, bounds.height)
        frame.minimumHeight = bounds.height
        win.attributes =
            win.attributes.apply {
                gravity = Gravity.BOTTOM or Gravity.LEFT
                x = bounds.left
                y = bounds.bottom + nav
                width = if (floating) bounds.width else WindowManager.LayoutParams.MATCH_PARENT
                height = bounds.height
                preferredRefreshRate = 120f
            }
        if (
            Build.VERSION.SDK_INT >= 31 &&
                getSystemService(WindowManager::class.java).isCrossWindowBlurEnabled
        ) {
            win.setBackgroundBlurRadius(
                dp(if (floating) T.Blur.backdropFloatingWindowPx else T.Blur.backdropStandardPx)
            )
        }
        sheet.background =
            GradientDrawable().apply {
                setColor(
                    (if (darkKeyboard) T.Dark.surfacePanelArgb else T.Light.surfacePanelArgb)
                        .toInt()
                )
                cornerRadius = dp(T.Radius.mobileKeyboardSheetPx).toFloat()
            }
    }

    override fun onComputeInsets(out: Insets) {
        super.onComputeInsets(out)
        if (floating && ::sheet.isInitialized) {
            val loc = IntArray(2)
            sheet.getLocationInWindow(loc)
            out.touchableInsets = Insets.TOUCHABLE_INSETS_REGION
            out.touchableRegion.set(loc[0], loc[1], loc[0] + sheet.width, loc[1] + sheet.height)
            out.contentTopInsets = window.window?.decorView?.height ?: frame.height
            out.visibleTopInsets = out.contentTopInsets
        }
    }

    private fun label(text: String, click: () -> Unit) =
        TextView(this).apply {
            this.text = text
            textSize = font
            setTextColor(
                (if (darkKeyboard) T.Dark.textPrimaryArgb else T.Light.textPrimaryArgb).toInt()
            )
            gravity = Gravity.CENTER
            setPadding(dp(6), 0, dp(6), 0)
            isFocusable = true
            setOnClickListener { click() }
        }

    private fun icon(parent: LinearLayout, id: Int, desc: String, click: () -> Unit): ImageButton {
        val button = ImageButton(this)
        button.setImageResource(id)
        button.setColorFilter(
            (if (darkKeyboard) T.Dark.accentTextArgb else T.Light.accentTextArgb).toInt()
        )
        button.contentDescription = desc
        button.setBackgroundColor(Color.TRANSPARENT)
        button.setOnClickListener { click() }
        button.setPadding(dp(5), dp(4), dp(5), dp(4))
        parent.addView(button, LinearLayout.LayoutParams(0, -1, 1f))
        return button
    }

    private fun drag(button: View, resize: Boolean) {
        var x = 0f
        var y = 0f
        var w = 0
        var h = 0
        var left = 0
        var bottom = 0
        button.setOnTouchListener { _, e ->
            when (e.actionMasked) {
                MotionEvent.ACTION_DOWN -> {
                    x = e.rawX
                    y = e.rawY
                    w = sheet.width
                    h = sheet.height
                    left = offsetX
                    bottom = offsetY
                }
                MotionEvent.ACTION_MOVE -> {
                    val dx = (e.rawX - x).toInt()
                    val dy = (e.rawY - y).toInt()
                    if (resize) {
                        floatWidth = (w + dx).coerceAtLeast(1)
                        floatHeight = (h - dy).coerceAtLeast(1)
                    } else {
                        offsetX = left + dx
                        offsetY = bottom - dy
                    }
                    layoutPanel()
                }
                MotionEvent.ACTION_UP -> button.performClick()
            }
            true
        }
    }

    private fun render() {
        if (!::sheet.isInitialized) return
        sheet.removeAllViews()
        val toolbar = LinearLayout(this)
        sheet.addView(toolbar, -1, dp(36))
        icon(toolbar, R.drawable.tools, "工具") {
            expanded = !expanded
            render()
        }
        icon(toolbar, R.drawable.language, if (english) "英文，切换为中文" else "中文，切换为英文") {
            call(
                "process-key",
                json("keycode" to "", "modifiers" to JSONArray().put("lang_toggle")),
            ) {
                applyAction(it)
            }
        }
        icon(toolbar, R.drawable.pen, "手写查音") { switch(if (mode == "lookup") "keys" else "lookup") }
        icon(toolbar, R.drawable.write, "连续手写") {
            switch(if (mode == "continuous") "keys" else "continuous")
        }
        icon(toolbar, R.drawable.window, "悬浮或停靠") {
            floating = !floating
            layoutPanel()
            render()
        }
        if (floating) {
            drag(icon(toolbar, R.drawable.move, "移动键盘") {}, false)
            drag(icon(toolbar, R.drawable.resize, "调整大小") {}, true)
        }
        if (expanded) {
            icon(toolbar, R.drawable.language, "翻译选中文本") { inlineAi("translate") }
            icon(toolbar, R.drawable.write, "润色选中文本") { inlineAi("polish") }
            icon(toolbar, R.drawable.clipboard, "剪贴板") { switch("clipboard") }
            icon(toolbar, R.drawable.settings, "设置") {
                startActivity(
                    Intent(this, SettingsActivity::class.java)
                        .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
                )
            }
        }
        val scroll = HorizontalScrollView(this).apply { isHorizontalScrollBarEnabled = false }
        bar = LinearLayout(this)
        scroll.addView(bar)
        sheet.addView(scroll, -1, dp(30))
        candidates()
        body = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        sheet.addView(body, -1, -1)
        when (mode) {
            "lookup",
            "continuous" -> {
                val gen = canvasGeneration
                body.addView(
                    InkView(this, mode == "continuous") { xs, ys, times ->
                        Core.run(
                            this,
                            operation = {
                                unpack(VitralNative.hwPushStroke(it, xs, ys, times, true))
                            },
                        ) { r ->
                            if (gen == canvasGeneration && active)
                                r.onSuccess { showInk(it as JSONObject) }
                                    .onFailure { message(it.message.orEmpty()) }
                        }
                    },
                    -1,
                    -1,
                )
            }
            "clipboard" -> call("clipboard-state") { if (mode == "clipboard") showRing(it) }
            else -> keyboard()
        }
    }

    private fun inlineAi(mode: String) {
        val original = currentInputConnection?.getSelectedText(0)?.toString().orEmpty()
        if (original.isBlank()) {
            message("请先在输入框选中原文")
            return
        }
        val request = ++inlineRequest
        val generation = editor
        bar.removeAllViews()
        bar.addView(
            label("处理中…，点击取消") {
                inlineRequest++
                candidates()
            }
        )
        Core.call(
            this,
            "ai-inline",
            json(
                "mode" to mode,
                "text" to original,
                "target_lang" to if (mode == "translate") "en" else "zh",
            ),
            true,
        ) { response ->
            if (!active || generation != editor || request != inlineRequest) return@call
            if (currentInputConnection?.getSelectedText(0)?.toString() != original) {
                candidates()
                return@call
            }
            response
                .onSuccess {
                    val replacement = (it as JSONObject).getString("output_text")
                    bar.removeAllViews()
                    bar.addView(
                        label(replacement) {
                            if (currentInputConnection?.getSelectedText(0)?.toString() == original)
                                currentInputConnection?.commitText(replacement, 1)
                            candidates()
                        }
                    )
                }
                .onFailure {
                    candidates()
                    message(it.message.orEmpty())
                }
        }
    }

    private fun switch(next: String) {
        inlineRequest++
        canvasGeneration++
        mode = next
        shown = JSONArray()
        currentInputConnection?.finishComposingText()
        composing = false
        Core.call(this, "process-key", json("keycode" to "Escape"))
        if (next == "lookup" || next == "continuous")
            call(
                "hw-begin",
                json("mode" to if (next == "lookup") "PhoneticLookup" else "ContinuousLattice"),
            )
        render()
    }

    private fun candidates() {
        if (!::bar.isInitialized) return
        bar.removeAllViews()
        if (paste.isNotEmpty())
            bar.addView(
                label("粘贴 " + paste.take(12)) {
                    call("consume-quick-paste") { r ->
                        r.optJSONObject("consumed")?.optString("content")?.let {
                            currentInputConnection?.commitText(it, 1)
                        }
                        updatePaste(r.getJSONObject("state"))
                    }
                }
            )
        if (mode == "lookup" || mode == "continuous") {
            bar.addView(label("清除") { switch(mode) })
            if (mode == "continuous")
                bar.addView(
                    label("确认") {
                        val gen = canvasGeneration
                        call("hw-end") { r ->
                            if (gen == canvasGeneration) {
                                currentInputConnection?.commitText(r.optString("composing_text"), 1)
                                switch(mode)
                            }
                        }
                    }
                )
            return
        }
        for (i in 0 until shown.length()) {
            val item = shown.getJSONObject(i)
            bar.addView(
                label(item.getString("text")) {
                        call("select-candidate", json("idx" to item.getInt("index"))) {
                            applyAction(it)
                        }
                    }
                    .apply {
                        setOnLongClickListener { v ->
                            PopupMenu(this@VitralIme, v)
                                .apply {
                                    menu.add("置顶").setOnMenuItemClickListener {
                                        call("pin-candidate", json("idx" to i)) { applyAction(it) }
                                        true
                                    }
                                    menu.add("移除").setOnMenuItemClickListener {
                                        call("erase-candidate", json("idx" to i)) {
                                            applyAction(it)
                                        }
                                        true
                                    }
                                }
                                .show()
                            true
                        }
                    }
            )
        }
    }

    private fun showRing(state: JSONObject) {
        val scroll = ScrollView(this)
        val list = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        scroll.addView(list)
        body.removeAllViews()
        body.addView(scroll)
        val ring = state.optJSONArray("ring") ?: return
        for (i in 0 until ring.length()) {
            val item = ring.getJSONObject(i)
            val row =
                label(item.getString("content")) {
                    currentInputConnection?.commitText(item.getString("content"), 1)
                    switch("keys")
                }
            row.setOnLongClickListener {
                list.removeAllViews()
                val code =
                    EditText(this).apply {
                        hint = "可选简码"
                        setText(item.optString("shortcut_code").takeUnless { it == "null" })
                    }
                list.addView(code)
                list.addView(
                    label("置顶并保存") {
                        call(
                            "clipboard-pin",
                            json(
                                "id" to item.getString("id"),
                                "shortcut_code" to code.text.toString(),
                            ),
                        ) {
                            showRing(it)
                        }
                    }
                )
                true
            }
            list.addView(row)
        }
        list.addView(label("清空未置顶") { call("clipboard-clear-unpinned") { showRing(it) } })
    }

    private fun showInk(r: JSONObject) {
        candidates()
        if (mode == "lookup") {
            val choices = r.optJSONArray("candidates") ?: return
            for (i in 0 until choices.length()) {
                val c = choices.getJSONObject(i)
                val text = c.getString("char")
                val code =
                    if (T.Handwriting.showXiaoheCodeInLookupMode) c.optString("xiaohe_code") else ""
                bar.addView(
                    label("$text  " + c.optString("toned_pinyin") + "  " + code) {
                        currentInputConnection?.commitText(text, 1)
                    }
                )
            }
        } else {
            currentInputConnection?.setComposingText(r.optString("composing_text"), 1)
            val slots = r.optJSONArray("slots") ?: return
            for (i in 0 until slots.length()) {
                val s = slots.getJSONObject(i)
                val choice =
                    label(s.getString("char")) {
                        bar.removeAllViews()
                        val options = s.getJSONArray("top8")
                        for (j in 0 until options.length()) bar.addView(
                            label(options.getJSONObject(j).getString("char")) {
                                hwOverrideSlot(i, j)
                            }
                        )
                    }
                if (s.optBoolean("changed_retroactively")) {
                    choice.setBackgroundColor(T.Dark.retroactiveFixGlowArgb.toInt())
                    choice.postDelayed(
                        { choice.setBackgroundColor(Color.TRANSPARENT) },
                        T.Handwriting.retroactiveHighlightDurationMs.toLong(),
                    )
                }
                bar.addView(choice)
            }
        }
    }

    private fun hwOverrideSlot(slot: Int, idx: Int) {
        val gen = canvasGeneration
        call("hw-override-slot", json("slot_idx" to slot, "candidate_idx" to idx)) {
            if (gen == canvasGeneration) showInk(it)
        }
    }

    private fun keyboard() {
        fun row(keys: List<String>, r: Int = -1) {
            val line = LinearLayout(this)
            body.addView(line, LinearLayout.LayoutParams(-1, 0, 1f))
            keys.forEachIndexed { c, s ->
                val key =
                    Key(
                        this,
                        s,
                        flypy,
                        if (r >= 0) alternate(r, c) else "",
                        ::key,
                        if (s == "空格") ::voice else null,
                    )
                line.addView(key, LinearLayout.LayoutParams(0, -1, if (s == "空格") 3f else 1f))
            }
        }
        if (numbers) {
            for (s in listOf("123", "456", "789")) row(s.map { it.toString() })
            row(listOf("ABC", "0", "⌫", "↵"))
            return
        }
        val count = if (flypy) T.Ergonomics.flypyRows else T.Ergonomics.quanpinRows
        if (count == 5) row("1234567890".map { it.toString() })
        rows.forEachIndexed { i, s -> row(s.map { it.toString() }, i) }
        row(listOf("123", "，", "空格", "。", "⌫", "↵"))
    }

    private fun key(label: String) {
        inlineRequest++
        val ic = currentInputConnection ?: return
        val vibrator = getSystemService(Vibrator::class.java)
        val duration =
            if (label.length == 1 && label[0].isLetter()) T.Ergonomics.hapticLetterTickMs
            else T.Ergonomics.hapticActionThudMs
        vibrator.vibrate(
            VibrationEffect.createOneShot(duration.toLong(), VibrationEffect.DEFAULT_AMPLITUDE)
        )
        when (label) {
            "123",
            "ABC" -> {
                numbers = !numbers
                render()
                return
            }
            "cancel-voice" -> {
                cancelVoice()
                return
            }
            "cursor-left",
            "cursor-right" -> {
                sendDownUpKeyEvents(
                    if (label == "cursor-left") KeyEvent.KEYCODE_DPAD_LEFT
                    else KeyEvent.KEYCODE_DPAD_RIGHT
                )
                return
            }
            "delete-word" -> {
                if (composing) call("process-key", json("keycode" to "Escape")) { applyAction(it) }
                else {
                    val before = ic.getTextBeforeCursor(96, 0)?.toString().orEmpty()
                    val n = before.takeLastWhile { it.isLetterOrDigit() }.length.coerceAtLeast(1)
                    ic.deleteSurroundingText(n, 0)
                }
                return
            }
        }
        val code =
            when (label) {
                "空格" -> "space"
                "⌫" -> "BackSpace"
                "↵" -> "Return"
                else -> label
            }
        call("process-key", json("keycode" to code)) { r ->
            when {
                !composing && label == "⌫" -> ic.deleteSurroundingTextInCodePoints(1, 0)
                !composing && label == "↵" ->
                    if (!sendDefaultEditorAction(true)) sendDownUpKeyEvents(KeyEvent.KEYCODE_ENTER)
                    else Unit
                else -> applyAction(r)
            }
        }
    }

    private fun applyAction(r: JSONObject) {
        val ic = currentInputConnection ?: return
        if (!r.isNull("commit_text")) ic.commitText(r.getString("commit_text"), 1)
        val pre = r.optJSONObject("preedit")?.optString("text").orEmpty()
        if (pre.isNotEmpty()) ic.setComposingText(pre, 1)
        else if (composing && r.isNull("commit_text")) ic.setComposingText("", 1)
        composing = pre.isNotEmpty()
        english = r.optString("lang_mode") == "en"
        shown = r.optJSONArray("candidates") ?: JSONArray()
        candidates()
    }

    private fun cancelVoice() {
        microphone.cancel()
    }

    private fun voice(start: Boolean) {
        if (!start) {
            microphone.stop()
            return
        }
        if (
            checkSelfPermission(Manifest.permission.RECORD_AUDIO) !=
                PackageManager.PERMISSION_GRANTED
        ) {
            message("请在设置中启用麦克风")
            return
        }
        val id = editor
        microphone.start(this) { result ->
            if (active && id == editor)
                result
                    .onSuccess { if (it.isNotEmpty()) currentInputConnection?.commitText(it, 1) }
                    .onFailure { message(it.message.orEmpty()) }
        }
    }
}
