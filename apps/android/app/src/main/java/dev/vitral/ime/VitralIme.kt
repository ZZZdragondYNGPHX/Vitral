package dev.vitral.ime

import android.Manifest
import android.content.ClipboardManager
import android.content.Intent
import android.content.pm.PackageManager
import android.graphics.Color
import android.graphics.Typeface
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
    private var hardwareShift = false
    private var shortcutKeys = setOf("Shift")
    private val handledHardwareKeys = mutableSetOf<Int>()
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
    private var pasteId = ""
    private var hasPreviousPage = false
    private var hasNextPage = false
    private var shown = JSONArray()
    private var lastInkResult: JSONObject? = null
    private var font = 16f
    private var floatWidth = 0
    private var floatHeight = 0
    private var offsetX = 0
    private var offsetY = 0
    private var systemCopyId = ""
    private var clipboardRevision = 0
    private var pasteVisible = false
    private val copyMarker = "dev.vitral.copy_id"
    private val microphone = Microphone()
    private val main = android.os.Handler(android.os.Looper.getMainLooper())

    private var alive = false
    private val pollClipboard =
        object : Runnable {
            override fun run() {
                if (!alive) return
                Core.call(this@VitralIme, "poll-services", background = true) {
                    if (alive) {
                        reconcileClipboard()
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

    override fun onStartInput(info: EditorInfo?, restarting: Boolean) {
        super.onStartInput(info, restarting)
        startEditor(info)
    }

    override fun onStartInputView(info: EditorInfo?, restarting: Boolean) {
        super.onStartInputView(info, restarting)
        if (!active) startEditor(info)
        layoutPanel()
        render()
    }

    private fun startEditor(info: EditorInfo?) {
        invalidateEditor()
        active = true
        mode = "keys"
        expanded = false
        shown = JSONArray()
        hasPreviousPage = false
        hasNextPage = false
        pasteId = ""
        composing = false
        val cfg = Core.settings(this)
        shortcutKeys =
            cfg.optJSONArray("shortcuts")?.let { rows ->
                (0 until rows.length())
                    .map { rows.getJSONObject(it).optString("key") }
                    .filter { it.isNotEmpty() }
                    .toSet()
            } ?: setOf("Shift")
        flypy = cfg.optString("schema", "double_pinyin_flypy") == "double_pinyin_flypy"
        font = cfg.optInt("candidate_font_px", 16).coerceIn(13, 22).toFloat()
        Core.call(this, "process-key", json("keycode" to "Escape"))
        call("on-app-focus", json("app_id" to (info?.packageName ?: "android"))) {
            english = it.optString("lang_mode") == "en"
            render()
        }
        call("on-ime-open") { updatePaste(it) }
        onPrimaryClipChanged()
        reconcileClipboard()
        layoutPanel()
        render()
    }

    override fun onFinishInputView(finishingInput: Boolean) {
        invalidateEditor()
        Core.call(this, "on-ime-close")
        super.onFinishInputView(finishingInput)
    }

    override fun onFinishInput() {
        invalidateEditor()
        Core.call(this, "on-ime-close")
        super.onFinishInput()
    }

    private fun invalidateEditor() {
        hardwareShift = false
        handledHardwareKeys.clear()
        inlineRequest++
        active = false
        editor++
        canvasGeneration++
        microphone.cancel()
    }

    private fun hardwareShortcut(code: Int, event: KeyEvent): String? {
        if (event.isMetaPressed) return null
        if (code == KeyEvent.KEYCODE_SHIFT_LEFT || code == KeyEvent.KEYCODE_SHIFT_RIGHT)
            return if (!event.isCtrlPressed && !event.isAltPressed) "Shift" else null
        val function = code in KeyEvent.KEYCODE_F1..KeyEvent.KEYCODE_F12
        if (!function && !event.isCtrlPressed && !event.isAltPressed) return null
        val base =
            when (code) {
                in KeyEvent.KEYCODE_F1..KeyEvent.KEYCODE_F12 ->
                    "F" + (code - KeyEvent.KEYCODE_F1 + 1)
                in KeyEvent.KEYCODE_A..KeyEvent.KEYCODE_Z ->
                    ('A'.code + code - KeyEvent.KEYCODE_A).toChar().toString()
                in KeyEvent.KEYCODE_0..KeyEvent.KEYCODE_9 -> (code - KeyEvent.KEYCODE_0).toString()
                KeyEvent.KEYCODE_SPACE -> "Space"
                KeyEvent.KEYCODE_ENTER -> "Enter"
                else -> return null
            }
        return listOfNotNull(
                if (event.isCtrlPressed) "Ctrl" else null,
                if (event.isAltPressed) "Alt" else null,
                if (event.isShiftPressed) "Shift" else null,
                base,
            )
            .joinToString("+")
    }

    override fun onKeyDown(keyCode: Int, event: KeyEvent): Boolean {
        if (currentInputConnection == null) return super.onKeyDown(keyCode, event)
        if (!active) startEditor(currentInputEditorInfo)
        val shortcut = hardwareShortcut(keyCode, event)
        if (keyCode == KeyEvent.KEYCODE_SHIFT_LEFT || keyCode == KeyEvent.KEYCODE_SHIFT_RIGHT) {
            hardwareShift = shortcut == "Shift" && shortcutKeys.contains("Shift")
            return super.onKeyDown(keyCode, event)
        }
        hardwareShift = false
        if (shortcut != null && shortcutKeys.contains(shortcut)) {
            if (event.repeatCount == 0)
                call("shortcut-key", json("key" to shortcut)) { applyAction(it) }
            handledHardwareKeys.add(keyCode)
            return true
        }
        if (!event.isCtrlPressed && !event.isAltPressed && !event.isMetaPressed && mode == "keys") {
            val text =
                when {
                    keyCode == KeyEvent.KEYCODE_DEL && (!english || composing || pasteVisible) ->
                        "⌫"
                    keyCode == KeyEvent.KEYCODE_ENTER && (!english || composing) -> "↵"
                    keyCode == KeyEvent.KEYCODE_SPACE && (!english || composing) -> "空格"
                    keyCode in KeyEvent.KEYCODE_A..KeyEvent.KEYCODE_Z && !english ->
                        event.unicodeChar.toChar().toString()
                    keyCode in KeyEvent.KEYCODE_0..KeyEvent.KEYCODE_9 && composing ->
                        event.unicodeChar.toChar().toString()
                    else -> null
                }
            if (text != null) {
                key(text)
                handledHardwareKeys.add(keyCode)
                return true
            }
            if (english && event.unicodeChar >= 32)
                call("hide-quick-paste-for-input") {
                    updatePaste(it)
                    refreshCandidates()
                }
        }
        return super.onKeyDown(keyCode, event)
    }

    override fun onKeyUp(keyCode: Int, event: KeyEvent): Boolean {
        if (keyCode == KeyEvent.KEYCODE_SHIFT_LEFT || keyCode == KeyEvent.KEYCODE_SHIFT_RIGHT) {
            val toggle = hardwareShift && hardwareShortcut(keyCode, event) == "Shift"
            hardwareShift = false
            if (toggle) {
                call("shortcut-key", json("key" to "Shift")) { applyAction(it) }
                return true
            }
        }
        if (handledHardwareKeys.remove(keyCode)) return true
        return super.onKeyUp(keyCode, event)
    }

    private fun call(
        cmd: String,
        args: JSONObject = JSONObject(),
        done: (JSONObject) -> Unit = {},
    ) {
        val id = editor
        Core.call(this, cmd, args) {
            if (active && editor == id)
                it.onSuccess { v -> if (v is JSONObject) done(v) }
                    .onFailure { e -> message(e.message.orEmpty()) }
        }
    }

    private fun message(text: String) {
        Toast.makeText(this, text, Toast.LENGTH_SHORT).show()
    }

    private fun updatePaste(state: JSONObject) {
        val visible = state.optBoolean("candidate_visible")
        val value = state.optJSONObject("quick_paste")?.optString("id").orEmpty()
        if (value != pasteId || visible != pasteVisible) {
            pasteId = value
            pasteVisible = visible
            refreshCandidates()
        }
    }

    private fun refreshCandidates() {
        if (mode != "keys") return
        val generation = canvasGeneration
        call("candidate-state") {
            if (mode == "keys" && generation == canvasGeneration) applyAction(it)
        }
    }

    private fun reconcileClipboard() {
        if (!alive) return
        onPrimaryClipChanged()
        val revision = clipboardRevision
        Core.call(this, "quick-paste-state") { result ->
            if (!alive || revision != clipboardRevision) return@call
            result.onSuccess { value ->
                val state = value as JSONObject
                val entry = state.optJSONObject("current_copy")
                if (entry != null) {
                    val id = entry.getString("id")
                    val current = clipboard.primaryClip
                    val description = current?.description
                    val ownId = description?.extras?.getString(copyMarker)
                    val text = current?.takeIf { it.itemCount > 0 }?.getItemAt(0)?.text?.toString()
                    val newerLocal =
                        ownId == null &&
                            (description?.timestamp ?: 0) > entry.optLong("timestamp_ms")
                    if (id != systemCopyId && !newerLocal) {
                        if (text != entry.getString("content") || ownId != id) {
                            val clip =
                                android.content.ClipData.newPlainText(
                                    "Vitral",
                                    entry.getString("content"),
                                )
                            clip.description.extras =
                                android.os.PersistableBundle().apply { putString(copyMarker, id) }
                            clipboard.setPrimaryClip(clip)
                        }
                        systemCopyId = id
                    }
                }
                if (active) updatePaste(state)
            }
        }
    }

    override fun onPrimaryClipChanged() {
        val clip = clipboard.primaryClip ?: return
        val description = clip.description
        val marker = description.extras?.getString(copyMarker)
        if (marker != null) {
            systemCopyId = marker
            return
        }
        val stamp = description.timestamp
        val prefs = getSharedPreferences("vitral", 0)
        if (prefs.getLong("clipboard_stamp", -1L) == stamp) return
        val text = clip.takeIf { it.itemCount > 0 }?.getItemAt(0)?.text?.toString() ?: return
        if (text.isEmpty()) return
        ++clipboardRevision
        val revision = clipboardRevision
        prefs.edit().putLong("clipboard_stamp", stamp).apply()
        Core.call(
            this,
            "clipboard-push",
            json("payload" to json("content" to text, "timestamp_ms" to stamp)),
            background = true,
        ) {
            if (alive && revision == clipboardRevision) reconcileClipboard()
        }
    }

    private fun pillDrawable(fillArgb: Long, radiusDp: Int, strokeArgb: Long? = null) =
        GradientDrawable().apply {
            setColor(fillArgb.toInt())
            cornerRadius = dp(radiusDp).toFloat()
            if (strokeArgb != null) setStroke(dp(1), strokeArgb.toInt())
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
                @Suppress("DEPRECATION") insets?.stableInsetBottom ?: 0
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
            pillDrawable(
                if (darkKeyboard) T.Dark.surfacePanelArgb else T.Light.surfacePanelArgb,
                T.Radius.mobileKeyboardSheetPx,
                if (darkKeyboard) T.Dark.borderSpecularArgb else T.Light.borderSpecularArgb,
            )
        sheet.setPadding(dp(4), dp(3), dp(4), dp(4))
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

    private fun label(
        text: String,
        highlight: Boolean = false,
        subtle: Boolean = false,
        click: () -> Unit,
    ) =
        TextView(this).apply {
            this.text = text
            textSize = if (subtle) (font - 2f).coerceAtLeast(12f) else font
            typeface = if (highlight) Typeface.DEFAULT_BOLD else Typeface.DEFAULT
            setTextColor(
                when {
                    highlight && darkKeyboard -> T.Dark.accentTextArgb
                    highlight -> T.Light.accentTextArgb
                    subtle && darkKeyboard -> T.Dark.textSecondaryArgb
                    subtle -> T.Light.textSecondaryArgb
                    darkKeyboard -> T.Dark.textPrimaryArgb
                    else -> T.Light.textPrimaryArgb
                }.toInt()
            )
            gravity = Gravity.CENTER
            setPadding(dp(10), dp(3), dp(10), dp(3))
            if (highlight) {
                background =
                    pillDrawable(
                        if (darkKeyboard) T.Dark.accentSoftArgb else T.Light.accentSoftArgb,
                        T.Radius.candidateActivePillPx,
                    )
            }
            isFocusable = true
            setOnClickListener { click() }
        }

    private fun icon(
        parent: LinearLayout,
        id: Int,
        desc: String,
        activeState: Boolean = false,
        click: () -> Unit,
    ): ImageButton {
        val button = ImageButton(this)
        button.setImageResource(id)
        button.setColorFilter(
            when {
                activeState && darkKeyboard -> T.Dark.accentTextArgb
                activeState -> T.Light.accentTextArgb
                darkKeyboard -> T.Dark.textPrimaryArgb
                else -> T.Light.textPrimaryArgb
            }.toInt()
        )
        button.alpha = if (activeState) 1f else 0.85f
        button.contentDescription = desc
        button.background =
            if (activeState)
                pillDrawable(
                    if (darkKeyboard) T.Dark.accentSoftArgb else T.Light.accentSoftArgb,
                    9,
                )
            else null
        button.setOnClickListener { click() }
        button.setPadding(dp(8), dp(5), dp(8), dp(5))
        val lp = LinearLayout.LayoutParams(dp(42), dp(30)).apply {
            setMargins(dp(2), dp(2), dp(2), dp(2))
        }
        parent.addView(button, lp)
        return button
    }

    private fun langBadge(parent: LinearLayout, click: () -> Unit) {
        val badge =
            TextView(this).apply {
                text = if (english) "En" else "中"
                textSize = 13f
                typeface = Typeface.DEFAULT_BOLD
                gravity = Gravity.CENTER
                contentDescription = if (english) "英文，切换为中文" else "中文，切换为英文"
                setTextColor(
                    when {
                        !english && darkKeyboard -> T.Dark.accentTextArgb
                        !english -> T.Light.accentTextArgb
                        darkKeyboard -> T.Dark.textPrimaryArgb
                        else -> T.Light.textPrimaryArgb
                    }.toInt()
                )
                background =
                    pillDrawable(
                        when {
                            !english && darkKeyboard -> T.Dark.accentSoftArgb
                            !english -> T.Light.accentSoftArgb
                            darkKeyboard -> T.Dark.surfaceKeyFunctionArgb
                            else -> T.Light.surfaceKeyFunctionArgb
                        },
                        9,
                    )
                setOnClickListener { click() }
            }
        val lp = LinearLayout.LayoutParams(dp(38), dp(28)).apply {
            setMargins(dp(2), dp(3), dp(2), dp(3))
        }
        parent.addView(badge, lp)
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

        val toolbarScroll =
            HorizontalScrollView(this).apply { isHorizontalScrollBarEnabled = false }
        val toolbar = LinearLayout(this).apply { gravity = Gravity.CENTER_VERTICAL }
        toolbarScroll.addView(toolbar)
        sheet.addView(toolbarScroll, -1, dp(36))

        icon(toolbar, R.drawable.tools, "工具", activeState = expanded) {
            expanded = !expanded
            render()
        }
        langBadge(toolbar) {
            call(
                "process-key",
                json("keycode" to "", "modifiers" to JSONArray().put("lang_toggle")),
            ) {
                applyAction(it)
            }
        }
        icon(toolbar, R.drawable.pen, "查词", activeState = mode == "lookup") {
            switch(if (mode == "lookup") "keys" else "lookup")
        }
        icon(toolbar, R.drawable.write, "手写", activeState = mode == "continuous") {
            switch(if (mode == "continuous") "keys" else "continuous")
        }
        icon(toolbar, R.drawable.clipboard, "剪贴板", activeState = mode == "clipboard") {
            switch(if (mode == "clipboard") "keys" else "clipboard")
        }
        icon(toolbar, R.drawable.window, "悬浮或停靠", activeState = floating) {
            floating = !floating
            layoutPanel()
            render()
        }
        if (floating) {
            drag(icon(toolbar, R.drawable.move, "移动键盘") {}, false)
            drag(icon(toolbar, R.drawable.resize, "调整大小") {}, true)
        }
        if (expanded) {
            icon(toolbar, R.drawable.translate, "翻译选中文本") { inlineAi("translate") }
            icon(toolbar, R.drawable.polish, "润色选中文本") { inlineAi("polish") }
            icon(toolbar, R.drawable.settings, "设置") {
                startActivity(
                    Intent(this, SettingsActivity::class.java)
                        .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
                )
            }
        }

        // Subtle divider between toolbar and candidate bar
        sheet.addView(
            View(this).apply {
                setBackgroundColor(if (darkKeyboard) 0x18FFFFFF else 0x140F172A)
            },
            -1,
            dp(1),
        )

        val scroll = HorizontalScrollView(this).apply { isHorizontalScrollBarEnabled = false }
        bar =
            LinearLayout(this).apply {
                gravity = Gravity.CENTER_VERTICAL
                setPadding(dp(4), dp(2), dp(4), dp(2))
            }
        scroll.addView(bar)
        sheet.addView(scroll, -1, dp(34))
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
            label("AI 处理中…（点此取消）", highlight = true) {
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
                        label("点击替换：$replacement", highlight = true) {
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
        lastInkResult = null
        hasPreviousPage = false
        hasNextPage = false
        currentInputConnection?.finishComposingText()
        composing = false
        Core.call(this, "process-key", json("keycode" to "Escape"))
        if (next == "lookup" || next == "continuous")
            call(
                "hw-begin",
                json("mode" to if (next == "lookup") "PhoneticLookup" else "ContinuousLattice"),
            )
        render()
        if (next == "keys") refreshCandidates()
    }

    private fun candidates() {
        if (!::bar.isInitialized) return
        bar.removeAllViews()
        if (mode == "lookup" || mode == "continuous") {
            bar.addView(label("清空", subtle = true) { switch(mode) })
            if (mode == "continuous")
                bar.addView(
                    label("确认上屏", highlight = true) {
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
            val isQuickPaste = item.has("quick_paste_id")
            val isFunc = item.has("function_action")
            val display =
                when (item.optString("function_action")) {
                    "settings" -> "⚙ 设置"
                    "clipboard" -> "剪贴板"
                    "lookup" -> "查词"
                    "handwriting" -> "手写"
                    "toggle_language" -> "中/英"
                    else ->
                        if (isQuickPaste)
                            "粘贴 · " + item.getString("text").replace(Regex("\\s+"), " ")
                        else item.getString("text")
                }
            val view =
                label(display, highlight = (i == 0 || isQuickPaste || isFunc)) {
                        call("select-candidate", json("idx" to item.getInt("index"))) {
                            applyAction(it)
                        }
                    }
                    .apply {
                        val maxW =
                            if (isQuickPaste)
                                (sheet.width.takeIf { it > 0 }
                                    ?: resources.displayMetrics.widthPixels) * 2 / 3
                            else
                                ((sheet.width.takeIf { it > 0 }
                                        ?: resources.displayMetrics.widthPixels) / 4)
                                    .coerceAtLeast(dp(48))
                        maxWidth = maxW
                        setSingleLine(true)
                        ellipsize = android.text.TextUtils.TruncateAt.END
                        contentDescription =
                            (if (isQuickPaste) "快捷粘贴：" else "") + item.getString("text")
                        setOnLongClickListener { v ->
                            if (isQuickPaste || isFunc) return@setOnLongClickListener false
                            PopupMenu(this@VitralIme, v)
                                .apply {
                                    menu.add("置顶此词").setOnMenuItemClickListener {
                                        call("pin-candidate", json("idx" to i)) { applyAction(it) }
                                        true
                                    }
                                    menu.add("移除此词").setOnMenuItemClickListener {
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
            val lp =
                LinearLayout.LayoutParams(
                        LinearLayout.LayoutParams.WRAP_CONTENT,
                        LinearLayout.LayoutParams.MATCH_PARENT,
                    )
                    .apply { setMargins(dp(2), 0, dp(2), 0) }
            bar.addView(view, lp)
        }
        if (hasPreviousPage)
            bar.addView(
                label("‹ 上页", subtle = true) {
                    call("process-key", json("keycode" to "PageUp")) { applyAction(it) }
                }
            )
        if (hasNextPage)
            bar.addView(
                label("下页 ›", subtle = true) {
                    call("process-key", json("keycode" to "PageDown")) { applyAction(it) }
                }
            )
    }

    private fun showRing(state: JSONObject) {
        val scroll = ScrollView(this)
        val list =
            LinearLayout(this).apply {
                orientation = LinearLayout.VERTICAL
                setPadding(dp(6), dp(4), dp(6), dp(6))
            }
        scroll.addView(list)
        body.removeAllViews()
        body.addView(scroll, -1, -1)

        val ring = state.optJSONArray("ring") ?: JSONArray()
        if (ring.length() == 0) {
            list.addView(label("暂无剪贴板记录", subtle = true) {})
            return
        }

        val headerRow = LinearLayout(this).apply { gravity = Gravity.CENTER_VERTICAL }
        headerRow.addView(
            TextView(this).apply {
                text = "共 ${ring.length()} 条 · 点击粘贴，长按设简码"
                textSize = 11f
                setTextColor(
                    (if (darkKeyboard) T.Dark.textSecondaryArgb else T.Light.textSecondaryArgb)
                        .toInt()
                )
            },
            LinearLayout.LayoutParams(0, -2, 1f),
        )
        headerRow.addView(
            label("清空未置顶", subtle = true) {
                call("clipboard-clear-unpinned") { showRing(it) }
            }
        )
        list.addView(headerRow)

        for (pinnedGroup in listOf(true, false)) {
            var headerAdded = false
            for (i in 0 until ring.length()) {
                val item = ring.getJSONObject(i)
                val isPinned = item.optBoolean("pinned")
                if (isPinned != pinnedGroup) continue
                if (!headerAdded) {
                    headerAdded = true
                    list.addView(
                        TextView(this).apply {
                            text = if (pinnedGroup) "置顶收藏" else "最近复制"
                            textSize = 11f
                            typeface = Typeface.DEFAULT_BOLD
                            setPadding(dp(4), dp(6), dp(4), dp(3))
                            setTextColor(
                                (if (darkKeyboard) T.Dark.textSecondaryArgb
                                    else T.Light.textSecondaryArgb)
                                    .toInt()
                            )
                        }
                    )
                }

                val card =
                    LinearLayout(this).apply {
                        orientation = LinearLayout.HORIZONTAL
                        gravity = Gravity.CENTER_VERTICAL
                        setPadding(dp(10), dp(6), dp(6), dp(6))
                        background =
                            pillDrawable(
                                if (darkKeyboard) T.Dark.surfaceKeyFunctionArgb
                                else T.Light.surfaceKeyFunctionArgb,
                                10,
                                if (isPinned)
                                    (if (darkKeyboard) T.Dark.accentSoftArgb
                                    else T.Light.accentSoftArgb)
                                else null,
                            )
                    }
                val content = item.getString("content")
                val codeText = item.optString("shortcut_code").takeUnless { it.isEmpty() || it == "null" }
                val textView =
                    TextView(this).apply {
                        text =
                            if (codeText != null) "[$codeText] ${content.replace('\n', ' ')}"
                            else content.replace('\n', ' ')
                        textSize = 13.5f
                        maxLines = 1
                        ellipsize = android.text.TextUtils.TruncateAt.END
                        setTextColor(
                            (if (darkKeyboard) T.Dark.textPrimaryArgb else T.Light.textPrimaryArgb)
                                .toInt()
                        )
                        setOnClickListener {
                            currentInputConnection?.commitText(content, 1)
                            switch("keys")
                        }
                        setOnLongClickListener {
                            list.removeAllViews()
                            val code =
                                EditText(this@VitralIme).apply {
                                    hint = "可选快捷简码（如 ksb）"
                                    setText(codeText.orEmpty())
                                    setTextColor(
                                        (if (darkKeyboard) T.Dark.textPrimaryArgb
                                            else T.Light.textPrimaryArgb)
                                            .toInt()
                                    )
                                }
                            list.addView(code)
                            val btns = LinearLayout(this@VitralIme)
                            btns.addView(
                                label("置顶并保存", highlight = true) {
                                    call(
                                        "clipboard-pin",
                                        json(
                                            "id" to item.getString("id"),
                                            "shortcut_code" to code.text.toString().trim(),
                                        ),
                                    ) {
                                        showRing(it)
                                    }
                                }
                            )
                            btns.addView(label("取消", subtle = true) { showRing(state) })
                            list.addView(btns)
                            true
                        }
                    }
                card.addView(textView, LinearLayout.LayoutParams(0, -2, 1f))

                val pinBtn =
                    ImageButton(this).apply {
                        setImageResource(R.drawable.pin)
                        setColorFilter(
                            (when {
                                    isPinned && darkKeyboard -> T.Dark.accentTextArgb
                                    isPinned -> T.Light.accentPrimaryArgb
                                    darkKeyboard -> T.Dark.textSecondaryArgb
                                    else -> T.Light.textSecondaryArgb
                                })
                                .toInt()
                        )
                        setBackgroundColor(Color.TRANSPARENT)
                        contentDescription = if (isPinned) "取消置顶" else "置顶"
                        setPadding(dp(6), dp(4), dp(6), dp(4))
                        setOnClickListener {
                            val cmd = if (isPinned) "clipboard-unpin" else "clipboard-pin"
                            call(cmd, json("id" to item.getString("id"))) { showRing(it) }
                        }
                    }
                card.addView(pinBtn, LinearLayout.LayoutParams(dp(34), dp(30)))

                val lp =
                    LinearLayout.LayoutParams(-1, -2).apply { setMargins(0, dp(2), 0, dp(2)) }
                list.addView(card, lp)
            }
        }
    }

    private fun showInk(r: JSONObject) {
        lastInkResult = r
        candidates()
        if (mode == "lookup") {
            val choices = r.optJSONArray("candidates") ?: return
            for (i in 0 until choices.length()) {
                val c = choices.getJSONObject(i)
                val text = c.getString("char")
                val py = c.optString("toned_pinyin")
                val code =
                    if (T.Handwriting.showXiaoheCodeInLookupMode) c.optString("xiaohe_code") else ""
                val badge = if (code.isNotEmpty()) "$text $py · $code" else "$text $py"
                bar.addView(
                    label(badge, highlight = i == 0) {
                        currentInputConnection?.commitText(text, 1)
                    }
                )
            }
        } else {
            currentInputConnection?.setComposingText(r.optString("composing_text"), 1)
            val slots = r.optJSONArray("slots") ?: return
            val glowColor =
                (if (darkKeyboard) T.Dark.retroactiveFixGlowArgb
                    else T.Light.retroactiveFixGlowArgb)
                    .toInt()
            for (i in 0 until slots.length()) {
                val s = slots.getJSONObject(i)
                val choice =
                    label(s.getString("char")) {
                        bar.removeAllViews()
                        bar.addView(
                            label("←", subtle = true) { lastInkResult?.let { showInk(it) } }
                        )
                        val options = s.getJSONArray("top8")
                        for (j in 0 until options.length()) bar.addView(
                            label(
                                options.getJSONObject(j).getString("char"),
                                highlight = j == 0,
                            ) {
                                hwOverrideSlot(i, j)
                            }
                        )
                    }
                if (s.optBoolean("changed_retroactively")) {
                    choice.background =
                        pillDrawable(
                            glowColor.toLong() and 0xFFFFFFFFL,
                            T.Radius.candidateActivePillPx,
                        )
                    choice.postDelayed(
                        { choice.background = null },
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
        fun roleFor(s: String): KeyRole =
            when (s) {
                "↵" -> KeyRole.ACCENT
                "123",
                "ABC",
                "⌫",
                "空格",
                "，",
                "。" -> KeyRole.FUNCTION
                else -> KeyRole.PRIMARY
            }

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
                        roleFor(s),
                    )
                val weight =
                    when (s) {
                        "空格" -> 3.2f
                        "↵",
                        "⌫",
                        "123",
                        "ABC" -> 1.25f
                        else -> 1f
                    }
                line.addView(key, LinearLayout.LayoutParams(0, -1, weight))
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
        val dismissPaste =
            label == "⌫" &&
                !composing &&
                shown.length() > 0 &&
                shown.getJSONObject(0).has("quick_paste_id")
        call("process-key", json("keycode" to code)) { r ->
            when {
                dismissPaste -> {
                    applyAction(r)
                    call("quick-paste-state") { updatePaste(it) }
                }
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
        val previousEnglish = english
        english = r.optString("lang_mode") == "en"
        shown = r.optJSONArray("candidates") ?: JSONArray()
        hasPreviousPage = r.optJSONObject("page")?.optBoolean("has_prev") ?: false
        hasNextPage = r.optJSONObject("page")?.optBoolean("has_next") ?: false
        if (english != previousEnglish) render() else candidates()
        when (r.optString("function_action")) {
            "settings" ->
                startActivity(
                    Intent(this, SettingsActivity::class.java)
                        .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
                )
            "clipboard" -> switch("clipboard")
            "lookup" -> switch("lookup")
            "handwriting" -> switch("continuous")
        }
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
