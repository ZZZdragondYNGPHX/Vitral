package dev.vitral.ime

import android.Manifest
import android.annotation.SuppressLint
import android.content.ClipboardManager
import android.content.Intent
import android.content.pm.PackageManager
import android.graphics.Color
import android.graphics.Typeface
import android.graphics.drawable.ColorDrawable
import android.graphics.drawable.GradientDrawable
import android.graphics.drawable.Icon
import android.inputmethodservice.InputMethodService
import android.os.Build
import android.os.Bundle
import android.os.SystemClock
import android.os.VibrationEffect
import android.os.Vibrator
import android.text.InputType
import android.text.SpannableStringBuilder
import android.text.Spanned
import android.text.style.ForegroundColorSpan
import android.text.style.StyleSpan
import android.util.Size
import android.view.*
import android.view.inputmethod.EditorInfo
import android.view.inputmethod.InlineSuggestionsRequest
import android.view.inputmethod.InlineSuggestionsResponse
import android.view.inputmethod.InputMethodManager
import android.widget.*
import android.widget.inline.InlinePresentationSpec
import androidx.autofill.inline.UiVersions
import androidx.autofill.inline.common.ImageViewStyle
import androidx.autofill.inline.common.TextViewStyle
import androidx.autofill.inline.common.ViewStyle
import androidx.autofill.inline.v1.InlineSuggestionUi
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
    private val imm by lazy { getSystemService(InputMethodManager::class.java) }
    private val vibrator by lazy { getSystemService(Vibrator::class.java) }
    private val mediumTypeface by lazy { Typeface.create("sans-serif-medium", Typeface.NORMAL) }
    private val boldTypeface by lazy { Typeface.create("sans-serif-medium", Typeface.BOLD) }
    private var hardwareShift = false
    private var shortcutKeys = setOf("Shift")
    private val handledHardwareKeys = mutableSetOf<Int>()
    private var inputGeneration = 0
    private val backspaceFlight = BackspaceFlightGate()
    private var hwComposing = false
    private val pressedPointers = mutableSetOf<Int>()
    private var renderPending = false
    private var loadedUiSettings = ""
    private val preferencesListener =
        android.content.SharedPreferences.OnSharedPreferenceChangeListener { _, key ->
            if (key == "settings" && active && Core.settings(this).toString() != loadedUiSettings) {
                loadUiPreferences()
                layoutPanel()
                render()
                refreshCandidates()
            }
        }
    private var editor = 0
    private var canvasGeneration = 0
    private var active = false
    private var floating = false
    private var flypy = true
    private var syllableSplit = false
    private var aeviouExperimental = false
    private var aeviouLeftHanded = false
    private var detectedAeviouEnv = AeviouEnv.MIXED
    private var manualLangOverride = false
    private var numbers = false
    private var mode = "keys" // keys | tools | lookup | continuous | clipboard | clipboard_shortcut | candidates | emoji | symbols
    private var clipboardItems: List<ClipboardItemSnapshot>? = null
    private var clipboardMutationSeq = 0
    private var clipboardScrollY = 0
    private var clipboardScrollView: ScrollView? = null
    private var editingShortcutItemId = ""
    private var editingShortcutContent = ""
    private var editingShortcutCode = ""
    private var shortcutCodeDisplay: TextView? = null
    private var shortcutClearBtn: TextView? = null
    private var inlineRequest = 0
    private var english = false
    private var fieldForceEnglish = false
    private var fieldKind = "text" // text | number | email | uri | password
    private var autoCapMode = 0 // 0=none, 1=sentences, 2=words, 3=all
    private var shiftState = 0 // 0=lower, 1=single, 2=capsLock
    private var lastShiftTapMs = 0L
    private var lastSpaceTapMs = 0L
    private var lastCommittedText = ""
    private var lastDeletedText = ""
    private var lastDeletedAtMs = 0L
    private var emojiCategory = 0
    private var symbolCategory = 0
    private var composing = false
    private var preeditText = ""
    private var preeditCursorUtf8 = 0
    private var passwordEditor = false
    private var enterLabel = "↵"
    private var themePref = "system" // system | dark | light
    private var sublabelMode = SublabelMode.DUAL
    private var heightPreset = "standard" // compact | standard | comfort
    private var predictionEnabled = true
    private var predictionDelay = 380L
    private var predictionEpoch = 0
    private var predictionId = 0L
    private var predictionActive = false
    private var predictionText = ""
    private var predictionCandidates: List<String> = emptyList()
    private var predictionContext = ""
    private var predictionContextAfter = ""
    private var predictionTask: Runnable? = null
    private var idleStripSignature: String? = null
    private var surroundingBeforeValid = false
    private var surroundingAfterValid = false
    private var cachedBefore = ""
    private var cachedAfter = ""
    private var aeviouHintEpoch = 0
    private var lastKeyboardCaseComposing: Boolean? = null
    private var lastKeyboardCaseEnglish: Boolean? = null
    private var lastKeyboardCaseShift = -1
    private var renderedTreeSignature: String? = null

    private class CandidateSlot(
        val view: TextView,
        var index: Int = 0,
        var canMenu: Boolean = false,
        var highlighted: Boolean = false,
        var quickPaste: Boolean = false,
        var highlightBg: GradientDrawable? = null,
    )

    private var composingStripActive = false
    private var composingStripDark: Boolean? = null
    private var composingStripFont = 0f
    private var stripPreeditView: TextView? = null
    private var stripDelimiterView: TextView? = null
    private var stripSplitView: TextView? = null
    private var stripSplitActive: Boolean? = null
    private var stripCandidatesContainer: LinearLayout? = null
    private val stripCandidateSlots = ArrayList<CandidateSlot>(12)
    private var stripPrevView: TextView? = null
    private var stripNextView: TextView? = null
    private var stripExpandView: TextView? = null
    private var pasteId = ""
    private var hasPreviousPage = false
    private var hasNextPage = false
    private var inlineSuggestionEpoch = 0
    private val inlineSuggestionViews = ArrayList<View>()
    private val inlinePinnedViews = ArrayList<View>()
    private var inlineSuggestionsActive = false
    private var shown = JSONArray()
    private var lastInkResult: JSONObject? = null
    private var hwModelReady: Boolean? = null
    private var hwErrorMessage = ""
    private var imeBottomInsetPx = 0
    private var imeLeftInsetPx = 0
    private var imeRightInsetPx = 0
    private var editSelectMode = false
    private var aeviouView: AeviouKeyboard? = null
    private var font = 16f
    private var floatWidth = 0
    private var floatHeight = 0
    private var offsetX = 0
    private var offsetY = 0
    private var systemCopyId = ""
    private var clipboardRevision = 0
    private var lastPushedClipText = ""
    private var pollServiceTick = 0
    private var pasteVisible = false
    private val copyMarker = "dev.vitral.copy_id"
    private val microphone = Microphone()
    private val main = android.os.Handler(android.os.Looper.getMainLooper())

    private val isLandscape: Boolean
        get() =
            resources.configuration.orientation ==
                android.content.res.Configuration.ORIENTATION_LANDSCAPE ||
                resources.displayMetrics.widthPixels > resources.displayMetrics.heightPixels

    private val isDarkTheme: Boolean
        get() =
            when (themePref) {
                "dark" -> true
                "light" -> false
                else -> darkKeyboard
            }

    private var alive = false
    private val pollClipboard =
        object : Runnable {
            override fun run() {
                if (!alive) return
                if (active) onPrimaryClipChanged()
                val revision = clipboardRevision
                val fullSync = pollServiceTick == 0 || pollServiceTick % 6 == 0
                pollServiceTick += 1
                val cmd = if (fullSync) "poll-services" else "clipboard-poll-current"
                Core.call(this@VitralIme, cmd, background = true) { result ->
                    if (alive) {
                        if (revision == clipboardRevision) {
                            result
                                .onSuccess { value ->
                                    (value as? JSONObject)?.let { applyClipboardState(it) }
                                }
                                .onFailure { reconcileClipboard() }
                        }
                        main.postDelayed(this, if (active) 1500 else 3000)
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
        getSharedPreferences("vitral", 0).registerOnSharedPreferenceChangeListener(preferencesListener)
    }

    override fun onDestroy() {
        alive = false
        clearInlineSuggestions(refreshBar = false)
        main.removeCallbacks(pollClipboard)
        invalidateEditor()
        clipboard.removePrimaryClipChangedListener(this)
        getSharedPreferences("vitral", 0).unregisterOnSharedPreferenceChangeListener(preferencesListener)
        super.onDestroy()
    }

    private fun clearInlineSuggestions(refreshBar: Boolean = false) {
        inlineSuggestionEpoch += 1
        inlineSuggestionViews.forEach { (it.parent as? ViewGroup)?.removeView(it) }
        inlinePinnedViews.forEach { (it.parent as? ViewGroup)?.removeView(it) }
        inlineSuggestionViews.clear()
        inlinePinnedViews.clear()
        val hadAny = inlineSuggestionsActive
        inlineSuggestionsActive = false
        idleStripSignature = null
        if (refreshBar && hadAny && alive && active && ::bar.isInitialized && mode == "keys" && !composing) {
            candidates()
        }
    }

    @SuppressLint("RestrictedApi")
    override fun onCreateInlineSuggestionsRequest(uiExtras: Bundle): InlineSuggestionsRequest? {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.R) return null
        val dark = isDarkTheme
        val chipBg =
            Icon.createWithResource(
                this,
                if (dark) R.drawable.inline_autofill_chip_dark else R.drawable.inline_autofill_chip_light,
            )
        val titleColor = (if (dark) T.Dark.textPrimaryArgb else T.Light.textPrimaryArgb).toInt()
        val subtitleColor = (if (dark) T.Dark.textSecondaryArgb else T.Light.textSecondaryArgb).toInt()
        val style =
            InlineSuggestionUi.newStyleBuilder()
                .setSingleIconChipStyle(
                    ViewStyle.Builder()
                        .setBackground(chipBg)
                        .setPadding(dp(8), 0, dp(8), 0)
                        .setLayoutMargin(dp(2), dp(2), dp(2), dp(2))
                        .build()
                )
                .setChipStyle(
                    ViewStyle.Builder()
                        .setBackground(chipBg)
                        .setPadding(dp(10), 0, dp(10), 0)
                        .setLayoutMargin(dp(3), dp(2), dp(3), dp(2))
                        .build()
                )
                .setStartIconStyle(
                    ImageViewStyle.Builder()
                        .setMaxWidth(dp(20))
                        .setMaxHeight(dp(20))
                        .setLayoutMargin(0, 0, dp(6), 0)
                        .build()
                )
                .setTitleStyle(
                    TextViewStyle.Builder()
                        .setTextColor(titleColor)
                        .setTextSize(13f)
                        .setTypeface("sans-serif-medium", Typeface.NORMAL)
                        .build()
                )
                .setSubtitleStyle(
                    TextViewStyle.Builder()
                        .setTextColor(subtitleColor)
                        .setTextSize(11.5f)
                        .setLayoutMargin(dp(4), 0, 0, 0)
                        .build()
                )
                .setEndIconStyle(
                    ImageViewStyle.Builder()
                        .setMaxWidth(dp(18))
                        .setMaxHeight(dp(18))
                        .setLayoutMargin(dp(6), 0, 0, 0)
                        .build()
                )
                .build()
        val stylesBundle = UiVersions.newStylesBuilder().addStyle(style).build()
        val maxChipWidth = (resources.displayMetrics.widthPixels * 3 / 4).coerceAtLeast(dp(180))
        val spec =
            InlinePresentationSpec.Builder(Size(dp(36), dp(28)), Size(maxChipWidth, dp(36)))
                .setStyle(stylesBundle)
                .build()
        return InlineSuggestionsRequest.Builder(listOf(spec))
            .setMaxSuggestionCount(6)
            .build()
    }

    override fun onInlineSuggestionsResponse(response: InlineSuggestionsResponse): Boolean {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.R) return false
        val suggestions = response.inlineSuggestions.take(6)
        val epoch = ++inlineSuggestionEpoch
        inlineSuggestionViews.forEach { (it.parent as? ViewGroup)?.removeView(it) }
        inlinePinnedViews.forEach { (it.parent as? ViewGroup)?.removeView(it) }
        inlineSuggestionViews.clear()
        inlinePinnedViews.clear()
        inlineSuggestionsActive = false
        idleStripSignature = null
        if (suggestions.isEmpty()) {
            if (alive && active && ::bar.isInitialized && mode == "keys" && !composing) {
                candidates()
            }
            return true
        }
        val inflated = arrayOfNulls<View>(suggestions.size)
        val pinned = BooleanArray(suggestions.size)
        val size = Size(ViewGroup.LayoutParams.WRAP_CONTENT, dp(34))
        suggestions.forEachIndexed { idx, suggestion ->
            pinned[idx] = suggestion.info.isPinned
            try {
                suggestion.inflate(this, size, mainExecutor) { contentView ->
                    if (!alive || epoch != inlineSuggestionEpoch) return@inflate
                    if (contentView != null) {
                        contentView.setZOrderedOnTop(true)
                        inflated[idx] = contentView
                        inlineSuggestionViews.clear()
                        inlinePinnedViews.clear()
                        for (i in inflated.indices) {
                            val v = inflated[i] ?: continue
                            if (pinned[i]) inlinePinnedViews.add(v) else inlineSuggestionViews.add(v)
                        }
                        inlineSuggestionsActive =
                            inlineSuggestionViews.isNotEmpty() || inlinePinnedViews.isNotEmpty()
                        idleStripSignature = null
                        if (active && ::bar.isInitialized && mode == "keys" && !composing) {
                            candidates()
                        }
                    }
                }
            } catch (_: Throwable) {
                // Ignore malformed remote inline suggestion from third-party provider
            }
        }
        return true
    }

    override fun onCreateInputView(): View {
        pressedPointers.clear()
        renderedTreeSignature = null
        frame = object : FrameLayout(this) {
            override fun onDetachedFromWindow() {
                pressedPointers.clear()
                renderPending = false
                super.onDetachedFromWindow()
            }

            override fun dispatchTouchEvent(event: MotionEvent): Boolean {
                when (event.actionMasked) {
                    MotionEvent.ACTION_DOWN, MotionEvent.ACTION_POINTER_DOWN ->
                        pressedPointers.add(event.getPointerId(event.actionIndex))
                    MotionEvent.ACTION_UP, MotionEvent.ACTION_POINTER_UP ->
                        pressedPointers.remove(event.getPointerId(event.actionIndex))
                    MotionEvent.ACTION_CANCEL -> pressedPointers.clear()
                }
                val handled = super.dispatchTouchEvent(event)
                if (pressedPointers.isEmpty() && renderPending) {
                    renderPending = false
                    render()
                }
                return handled
            }
        }
        sheet = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        frame.addView(sheet)
        frame.setOnApplyWindowInsetsListener { _, insets ->
            val bottom: Int
            val left: Int
            val right: Int
            if (Build.VERSION.SDK_INT >= 30) {
                val nav = insets.getInsets(WindowInsets.Type.navigationBars())
                val tap = insets.getInsets(WindowInsets.Type.tappableElement())
                val gesture = insets.getInsets(WindowInsets.Type.mandatorySystemGestures())
                val cutout = insets.getInsets(WindowInsets.Type.displayCutout())
                bottom = maxOf(nav.bottom, tap.bottom, gesture.bottom)
                left = maxOf(nav.left, cutout.left)
                right = maxOf(nav.right, cutout.right)
            } else {
                @Suppress("DEPRECATION")
                bottom = insets.systemWindowInsetBottom
                val cutout = insets.displayCutout
                @Suppress("DEPRECATION")
                left = maxOf(insets.systemWindowInsetLeft, cutout?.safeInsetLeft ?: 0)
                @Suppress("DEPRECATION")
                right = maxOf(insets.systemWindowInsetRight, cutout?.safeInsetRight ?: 0)
            }
            if (bottom != imeBottomInsetPx || left != imeLeftInsetPx || right != imeRightInsetPx) {
                imeBottomInsetPx = bottom
                imeLeftInsetPx = left
                imeRightInsetPx = right
                layoutPanel()
            }
            insets
        }
        layoutPanel()
        render()
        return frame
    }

    override fun onConfigurationChanged(newConfig: android.content.res.Configuration) {
        super.onConfigurationChanged(newConfig)
        imeBottomInsetPx = 0
        imeLeftInsetPx = 0
        imeRightInsetPx = 0
        renderedTreeSignature = null
        if (::frame.isInitialized) {
            layoutPanel()
            render()
        }
    }

    override fun onStartInput(info: EditorInfo?, restarting: Boolean) {
        super.onStartInput(info, restarting)
        if (!restarting) clearInlineSuggestions(refreshBar = false)
        startEditor(info)
    }

    override fun onStartInputView(info: EditorInfo?, restarting: Boolean) {
        super.onStartInputView(info, restarting)
        if (!active) startEditor(info)
        layoutPanel()
        render()
    }

    override fun onWindowShown() {
        super.onWindowShown()
        // On Android 10+, primaryClip becomes readable once the IME window gains focus.
        onPrimaryClipChanged()
    }

    private fun resolveEnterLabel(info: EditorInfo?): String {
        val imeOptions = info?.imeOptions ?: 0
        if (imeOptions and EditorInfo.IME_FLAG_NO_ENTER_ACTION != 0) return "↵"
        return when (imeOptions and EditorInfo.IME_MASK_ACTION) {
            EditorInfo.IME_ACTION_SEARCH -> "搜索"
            EditorInfo.IME_ACTION_SEND -> "发送"
            EditorInfo.IME_ACTION_GO -> "前往"
            EditorInfo.IME_ACTION_NEXT -> "下一项"
            EditorInfo.IME_ACTION_DONE -> "完成"
            else -> "↵"
        }
    }

    private fun loadUiPreferences() {
        val cfg = Core.settings(this)
        loadedUiSettings = cfg.toString()
        predictionEnabled = cfg.optBoolean("prediction_enabled", true)
        predictionDelay = cfg.optLong("prediction_delay_ms", 380).coerceIn(150, 2000)
        shortcutKeys =
            cfg.optJSONArray("shortcuts")?.let { rows ->
                (0 until rows.length())
                    .map { rows.getJSONObject(it).optString("key") }
                    .filter { it.isNotEmpty() }
                    .toSet()
            } ?: setOf("Shift")
        val nextFlypy = cfg.optString("schema", "double_pinyin_flypy") == "double_pinyin_flypy"
        if (nextFlypy != flypy) syllableSplit = false
        flypy = nextFlypy
        aeviouExperimental = cfg.optBoolean("mobile_experimental_aeviou", false)
        aeviouLeftHanded = cfg.optString("mobile_aeviou_hand", "right") == "left"
        font = cfg.optInt("candidate_font_px", 16).coerceIn(13, 22).toFloat()
        themePref = cfg.optString("theme", "system")
        sublabelMode =
            when (cfg.optString("mobile_sublabel_mode", "dual")) {
                "symbols" -> SublabelMode.SYMBOLS
                "clean" -> SublabelMode.CLEAN
                else -> SublabelMode.DUAL
            }
        heightPreset = cfg.optString("mobile_keyboard_height", "standard")
    }

    private fun startEditor(info: EditorInfo?) {
        invalidateEditor()
        backspaceFlight.reset()
        active = true
        mode = "keys"
        numbers = false
        shiftState = 0
        syllableSplit = false
        shown = JSONArray()
        hasPreviousPage = false
        hasNextPage = false
        pasteId = ""
        composing = false
        hwComposing = false
        preeditText = ""
        preeditCursorUtf8 = 0
        enterLabel = resolveEnterLabel(info)
        val inputType = info?.inputType ?: 0
        val inputClass = inputType and InputType.TYPE_MASK_CLASS
        val variation = inputType and InputType.TYPE_MASK_VARIATION
        passwordEditor =
            when (inputClass) {
                InputType.TYPE_CLASS_TEXT ->
                    variation in
                        setOf(
                            InputType.TYPE_TEXT_VARIATION_PASSWORD,
                            InputType.TYPE_TEXT_VARIATION_VISIBLE_PASSWORD,
                            InputType.TYPE_TEXT_VARIATION_WEB_PASSWORD,
                        )
                InputType.TYPE_CLASS_NUMBER -> variation == InputType.TYPE_NUMBER_VARIATION_PASSWORD
                else -> false
            }
        fieldKind =
            when {
                passwordEditor -> "password"
                inputClass in
                    setOf(
                        InputType.TYPE_CLASS_NUMBER,
                        InputType.TYPE_CLASS_PHONE,
                        InputType.TYPE_CLASS_DATETIME,
                    ) -> "number"
                inputClass == InputType.TYPE_CLASS_TEXT &&
                    variation in
                        setOf(
                            InputType.TYPE_TEXT_VARIATION_EMAIL_ADDRESS,
                            InputType.TYPE_TEXT_VARIATION_WEB_EMAIL_ADDRESS,
                        ) -> "email"
                inputClass == InputType.TYPE_CLASS_TEXT &&
                    variation == InputType.TYPE_TEXT_VARIATION_URI -> "uri"
                else -> "text"
            }
        fieldForceEnglish = passwordEditor
        numbers = fieldKind == "number"
        autoCapMode =
            when {
                fieldKind != "text" -> 0
                inputType and InputType.TYPE_TEXT_FLAG_CAP_CHARACTERS != 0 -> 3
                inputType and InputType.TYPE_TEXT_FLAG_CAP_WORDS != 0 -> 2
                inputType and InputType.TYPE_TEXT_FLAG_CAP_SENTENCES != 0 -> 1
                else -> 1
            }
        lastCommittedText = ""
        lastDeletedText = ""
        lastSpaceTapMs = 0L
        manualLangOverride = false
        loadUiPreferences()
        Core.run(this, operation = { unpack(VitralNative.processKey(it, 27, 0, 0)) })
        call("on-app-focus", json("app_id" to (info?.packageName ?: "android"))) {
            english = fieldForceEnglish || it.optString("lang_mode") == "en"
            if (!flypy && aeviouExperimental) {
                detectedAeviouEnv = detectAeviouEnvFromContext()
                syncAutoAeviouEnv(triggerLangSync = true, forceRender = false)
            }
            updateAutoCap(redraw = false)
            render()
            if (!passwordEditor) schedulePrediction("catch_up", 0)
        }
        call("on-ime-open") { updatePaste(it) }
        onPrimaryClipChanged()
        reconcileClipboard()
        layoutPanel()
        render()
    }

    override fun onUpdateSelection(
        oldSelStart: Int,
        oldSelEnd: Int,
        newSelStart: Int,
        newSelEnd: Int,
        candidatesStart: Int,
        candidatesEnd: Int,
    ) {
        super.onUpdateSelection(
            oldSelStart,
            oldSelEnd,
            newSelStart,
            newSelEnd,
            candidatesStart,
            candidatesEnd,
        )
        if (!active) return
        val moved = oldSelStart != newSelStart || oldSelEnd != newSelEnd
        if (moved) invalidateSurroundingCache()
        if (moved && !composing) {
            if (predictionText.isNotEmpty()) cancelPrediction()
            if (lastDeletedText.isNotEmpty() && SystemClock.uptimeMillis() - lastDeletedAtMs > 400L) {
                lastDeletedText = ""
                candidates()
            }
            updateAutoCap(redraw = true)
            if (!syncAutoAeviouEnv(triggerLangSync = true)) {
                refreshAeviouContextHints()
            }
        }
    }

    private fun invalidateSurroundingCache() {
        surroundingBeforeValid = false
        surroundingAfterValid = false
        cachedBefore = ""
        cachedAfter = ""
    }

    private fun readBeforeCursor(
        ic: android.view.inputmethod.InputConnection?,
        maxLen: Int,
    ): String {
        if (passwordEditor || ic == null || maxLen <= 0) return ""
        if (!surroundingBeforeValid) {
            if (Build.VERSION.SDK_INT >= 31) {
                val st = ic.getSurroundingText(48, 16, 0)
                if (st != null) {
                    val text = st.text
                    val selStart = st.selectionStart.coerceIn(0, text.length)
                    val selEnd = st.selectionEnd.coerceIn(selStart, text.length)
                    cachedBefore = text.subSequence(0, selStart).toString()
                    cachedAfter = text.subSequence(selEnd, text.length).toString()
                    surroundingBeforeValid = true
                    surroundingAfterValid = true
                } else {
                    cachedBefore = ic.getTextBeforeCursor(48, 0)?.toString().orEmpty()
                    surroundingBeforeValid = true
                    surroundingAfterValid = false
                }
            } else {
                cachedBefore = ic.getTextBeforeCursor(48, 0)?.toString().orEmpty()
                surroundingBeforeValid = true
                surroundingAfterValid = false
            }
        }
        return if (cachedBefore.length <= maxLen) cachedBefore
        else cachedBefore.substring(cachedBefore.length - maxLen)
    }

    private fun readAfterCursor(ic: android.view.inputmethod.InputConnection?): String {
        if (passwordEditor || ic == null) return ""
        if (!surroundingBeforeValid) readBeforeCursor(ic, 48)
        if (!surroundingAfterValid) {
            cachedAfter = ic.getTextAfterCursor(16, 0)?.toString().orEmpty()
            surroundingAfterValid = true
        }
        return cachedAfter
    }

    private fun updateAutoCap(redraw: Boolean) {
        if (!english || fieldKind != "text" || passwordEditor || shiftState == 2 || autoCapMode == 0)
            return
        val before = readBeforeCursor(currentInputConnection, 4)
        val shouldCap =
            when (autoCapMode) {
                3 -> true
                2 -> before.isEmpty() || before.last().isWhitespace()
                else ->
                    before.isEmpty() ||
                        before.endsWith("\n") ||
                        before.endsWith(". ") ||
                        before.endsWith("! ") ||
                        before.endsWith("? ")
            }
        val next = if (shouldCap) 1 else 0
        if (next != shiftState) {
            shiftState = next
            if (redraw && mode == "keys" && !numbers) refreshKeyboardCase()
        }
    }

    override fun onFinishInputView(finishingInput: Boolean) {
        invalidateEditor()
        Core.call(this, "on-ime-close")
        Core.call(this, "llm-unload", auxiliary = true)
        super.onFinishInputView(finishingInput)
    }

    override fun onFinishInput() {
        clearInlineSuggestions(refreshBar = false)
        invalidateEditor()
        Core.call(this, "on-ime-close")
        super.onFinishInput()
    }

    override fun onTrimMemory(level: Int) {
        cancelPrediction()
        Core.call(this, "llm-unload", auxiliary = true)
        super.onTrimMemory(level)
    }

    private fun clearShortcutEditorState() {
        editingShortcutItemId = ""
        editingShortcutContent = ""
        editingShortcutCode = ""
        shortcutCodeDisplay = null
        shortcutClearBtn = null
    }

    private fun invalidateEditor() {
        invalidateSurroundingCache()
        backspaceFlight.reset()
        hwComposing = false
        cancelPrediction()
        clearShortcutEditorState()
        clipboardScrollView = null
        hardwareShift = false
        handledHardwareKeys.clear()
        inlineRequest++
        active = false
        editor++
        inputGeneration++
        canvasGeneration++
        microphone.cancel()
    }

    private fun cancelPrediction() {
        predictionEpoch++
        predictionTask?.let { main.removeCallbacks(it) }
        predictionTask = null
        val visible = predictionText.isNotEmpty() || predictionCandidates.isNotEmpty()
        val hadRemote = predictionActive || predictionId != 0L || visible
        predictionText = ""
        predictionCandidates = emptyList()
        predictionContextAfter = ""
        predictionId = 0
        predictionActive = false
        if (hadRemote) Core.call(this, "predict-cancel", auxiliary = true)
        if (visible) candidates()
    }

    private fun schedulePrediction(trigger: String, delay: Long = predictionDelay) {
        cancelPrediction()
        if (!active || composing || passwordEditor || !predictionEnabled || mode != "keys") return
        val epoch = predictionEpoch
        val session = editor
        val task = Runnable {
            if (!active || session != editor || epoch != predictionEpoch || composing || passwordEditor)
                return@Runnable
            // Never request surrounding text from a password editor.
            val ic = currentInputConnection
            val before = ic?.getTextBeforeCursor(200, 0)?.toString().orEmpty()
            val after = ic?.getTextAfterCursor(80, 0)?.toString().orEmpty()
            predictionContext = before
            predictionContextAfter = after
            predictionActive = true
            call(
                "predict-request",
                json(
                    "app_id" to (currentInputEditorInfo?.packageName ?: "android"),
                    "context_before" to before,
                    "context_after" to after,
                    "trigger" to trigger,
                    "is_composing" to false,
                    "is_password" to false,
                ),
                auxiliary = true,
            ) {
                receivePrediction(it, epoch, session, 0)
            }
        }
        predictionTask = task
        main.postDelayed(task, delay)
    }

    private fun parsePredictionCandidates(r: JSONObject): List<String> {
        val list = ArrayList<String>(4)
        val arr = r.optJSONArray("candidates")
        if (arr != null) {
            for (i in 0 until arr.length()) {
                val c = arr.optString(i)
                if (c.isNotEmpty() && c !in list) list.add(c)
            }
        }
        val primary = r.optString("text")
        if (primary.isNotEmpty() && primary !in list) list.add(0, primary)
        return list.take(4)
    }

    private fun receivePrediction(r: JSONObject, epoch: Int, session: Int, polls: Int) {
        if (!active || session != editor || epoch != predictionEpoch || composing || passwordEditor || mode != "keys")
            return
        predictionId = r.optLong("id")
        when (r.optString("status")) {
            "pending" -> {
                if (polls >= 25) {
                    cancelPrediction()
                    return
                }
                val task = Runnable {
                    if (session == editor && epoch == predictionEpoch && !composing && !passwordEditor)
                        call("predict-poll", json("id" to predictionId), auxiliary = true) {
                            receivePrediction(it, epoch, session, polls + 1)
                        }
                }
                predictionTask = task
                main.postDelayed(task, 120)
            }
            "ready" -> {
                val ic = currentInputConnection
                if (
                    ic?.getTextBeforeCursor(200, 0)?.toString().orEmpty() != predictionContext ||
                        ic?.getTextAfterCursor(80, 0)?.toString().orEmpty() != predictionContextAfter
                ) {
                    cancelPrediction()
                    return
                }
                predictionCandidates = parsePredictionCandidates(r)
                predictionText = predictionCandidates.firstOrNull().orEmpty()
                predictionActive = predictionText.isNotEmpty()
                candidates()
                refreshAeviouContextHints()
            }
            else -> {
                predictionActive = false
            }
        }
    }

    private fun acceptPrediction(choice: String = predictionText) {
        if (passwordEditor || composing || choice.isEmpty()) return
        val ic = currentInputConnection ?: return
        if (
            ic.getTextBeforeCursor(200, 0)?.toString().orEmpty() != predictionContext ||
                ic.getTextAfterCursor(80, 0)?.toString().orEmpty() != predictionContextAfter
        ) {
            cancelPrediction()
            return
        }
        val text = choice
        val id = predictionId
        val epoch = predictionEpoch
        call("predict-poll", json("id" to id), auxiliary = true) { current ->
            if (epoch != predictionEpoch || predictionId != id || passwordEditor || composing) return@call
            val liveCandidates = parsePredictionCandidates(current)
            val liveIc = currentInputConnection
            if (
                current.optString("status") != "ready" ||
                    text !in liveCandidates ||
                    liveIc?.getTextBeforeCursor(200, 0)?.toString().orEmpty() != predictionContext ||
                    liveIc?.getTextAfterCursor(80, 0)?.toString().orEmpty() != predictionContextAfter
            ) {
                cancelPrediction()
                return@call
            }
            if (liveIc?.commitText(text, 1) != true) return@call
            invalidateSurroundingCache()
            Core.call(this, "predict-accept", json("id" to id, "text" to text), auxiliary = true)
            cancelPrediction()
            schedulePrediction("live")
        }
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
        if (
            !composing &&
                predictionText.isNotEmpty() &&
                keyCode == KeyEvent.KEYCODE_TAB &&
                !event.isShiftPressed &&
                !event.isCtrlPressed &&
                !event.isAltPressed &&
                !event.isMetaPressed
        ) {
            acceptPrediction()
            handledHardwareKeys.add(keyCode)
            return true
        }
        if (keyCode == KeyEvent.KEYCODE_ESCAPE && predictionText.isNotEmpty()) {
            Core.call(this, "predict-dismiss", json("id" to predictionId), auxiliary = true)
            cancelPrediction()
            handledHardwareKeys.add(keyCode)
            return true
        }
        cancelPrediction()
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
        if (mode == "clipboard_shortcut" && !event.isCtrlPressed && !event.isAltPressed && !event.isMetaPressed) {
            when (keyCode) {
                in KeyEvent.KEYCODE_A..KeyEvent.KEYCODE_Z -> {
                    onShortcutEditorKey(('a'.code + keyCode - KeyEvent.KEYCODE_A).toChar().toString())
                    handledHardwareKeys.add(keyCode)
                    return true
                }
                KeyEvent.KEYCODE_DEL -> {
                    onShortcutEditorKey("⌫")
                    handledHardwareKeys.add(keyCode)
                    return true
                }
                KeyEvent.KEYCODE_ENTER,
                KeyEvent.KEYCODE_NUMPAD_ENTER -> {
                    saveEditingShortcutCode()
                    handledHardwareKeys.add(keyCode)
                    return true
                }
                KeyEvent.KEYCODE_ESCAPE -> {
                    returnToClipboardFromShortcutEditor()
                    handledHardwareKeys.add(keyCode)
                    return true
                }
            }
        }
        if (!event.isCtrlPressed && !event.isAltPressed && !event.isMetaPressed && mode == "keys") {
            if (composing) {
                val navKey =
                    when (keyCode) {
                        KeyEvent.KEYCODE_DPAD_LEFT -> 0xff51
                        KeyEvent.KEYCODE_DPAD_RIGHT -> 0xff53
                        KeyEvent.KEYCODE_MOVE_HOME -> 0xff50
                        KeyEvent.KEYCODE_MOVE_END -> 0xff57
                        KeyEvent.KEYCODE_FORWARD_DEL -> 0xffff
                        else -> null
                    }
                if (navKey != null) {
                    processKeyDirect(navKey) { applyAction(it) }
                    handledHardwareKeys.add(keyCode)
                    return true
                }
            }
            val text =
                when {
                    keyCode == KeyEvent.KEYCODE_DEL && (!english || composing || pasteVisible) -> "⌫"
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
        auxiliary: Boolean = false,
        onComplete: () -> Unit = {},
        onSameEditorPanelSwitched: ((JSONObject) -> Unit)? = null,
        done: (JSONObject) -> Unit = {},
    ) {
        val id = editor
        val generation = inputGeneration
        Core.call(this, cmd, args, auxiliary = auxiliary) {
            onComplete()
            if (active && editor == id) {
                if (generation == inputGeneration) {
                    it.onSuccess { v -> if (v is JSONObject) done(v) }
                        .onFailure { e -> message(e.message.orEmpty()) }
                } else {
                    it.onSuccess { v ->
                        val r = v as? JSONObject ?: return@onSuccess
                        val committed =
                            if (r.has("commit_text") && !r.isNull("commit_text"))
                                r.optString("commit_text")
                            else ""
                        if (committed.isNotEmpty()) {
                            currentInputConnection?.commitText(committed, 1)
                            invalidateSurroundingCache()
                            if (!passwordEditor) lastCommittedText = committed
                        }
                        onSameEditorPanelSwitched?.invoke(r)
                    }
                }
            }
        }
    }

    private fun runNativeAction(
        onComplete: () -> Unit = {},
        onSameEditorPanelSwitched: ((JSONObject) -> Unit)? = null,
        operation: (Long) -> JSONObject,
        done: (JSONObject) -> Unit = {},
    ) {
        val id = editor
        val generation = inputGeneration
        Core.run(this, operation = operation) {
            onComplete()
            if (active && editor == id) {
                if (generation == inputGeneration) {
                    it.onSuccess { v -> if (v is JSONObject) done(v) }
                        .onFailure { e -> message(e.message.orEmpty()) }
                } else {
                    it.onSuccess { v ->
                        val r = v as? JSONObject ?: return@onSuccess
                        val committed =
                            if (r.has("commit_text") && !r.isNull("commit_text"))
                                r.optString("commit_text")
                            else ""
                        if (committed.isNotEmpty()) {
                            currentInputConnection?.commitText(committed, 1)
                            invalidateSurroundingCache()
                            if (!passwordEditor) lastCommittedText = committed
                        }
                        onSameEditorPanelSwitched?.invoke(r)
                    }
                }
            }
        }
    }

    private fun nativeKeycode(key: String): Int =
        when (key) {
            "space", "Space" -> 32
            "Return", "Enter" -> 13
            "Escape" -> 27
            "BackSpace", "Backspace" -> 8
            "Page_Up", "PageUp" -> 0xff55
            "Page_Down", "PageDown" -> 0xff56
            "" -> 0
            else ->
                if (key.codePointCount(0, key.length) == 1) key.codePointAt(0)
                else -1
        }

    private fun processKeyDirect(
        keycode: Int,
        modifiers: Int = 0,
        swipeDir: Int = 0,
        onComplete: () -> Unit = {},
        onSameEditorPanelSwitched: ((JSONObject) -> Unit)? = null,
        done: (JSONObject) -> Unit = {},
    ) =
        runNativeAction(
            onComplete = onComplete,
            onSameEditorPanelSwitched = onSameEditorPanelSwitched,
            operation = { h -> unpack(VitralNative.processKey(h, keycode, modifiers, swipeDir)) as JSONObject },
            done = done,
        )

    private fun selectCandidateDirect(
        index: Int,
        done: (JSONObject) -> Unit = {},
    ) =
        runNativeAction(
            operation = { h -> unpack(VitralNative.selectCandidate(h, index)) as JSONObject },
            done = done,
        )

    private fun pinCandidateDirect(
        index: Int,
        done: (JSONObject) -> Unit = {},
    ) =
        runNativeAction(
            operation = { h -> unpack(VitralNative.pinCandidate(h, index)) as JSONObject },
            done = done,
        )

    private fun eraseCandidateDirect(
        index: Int,
        done: (JSONObject) -> Unit = {},
    ) =
        runNativeAction(
            operation = { h -> unpack(VitralNative.eraseCandidate(h, index)) as JSONObject },
            done = done,
        )

    private fun message(text: String) {
        if (text.isNotBlank()) Toast.makeText(this, text, Toast.LENGTH_SHORT).show()
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

    private fun applyClipboardState(state: JSONObject) {
        val entry = state.optJSONObject("current_copy")
        if (entry != null) {
            val id = entry.getString("id")
            if (id != systemCopyId) {
                val current = clipboard.primaryClip
                val description = current?.description
                val ownId = description?.extras?.getString(copyMarker)
                val text = current?.takeIf { it.itemCount > 0 }?.getItemAt(0)?.text?.toString()
                val newerLocal =
                    ownId == null &&
                        (description?.timestamp ?: 0) > entry.optLong("timestamp_ms")
                if (!newerLocal) {
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
        }
        if (active) updatePaste(state)
    }

    private fun reconcileClipboard() {
        if (!alive) return
        onPrimaryClipChanged()
        val revision = clipboardRevision
        Core.call(this, "quick-paste-state", background = true) { result ->
            if (!alive || revision != clipboardRevision) return@call
            result.onSuccess { value ->
                (value as? JSONObject)?.let { applyClipboardState(it) }
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
        val rawStamp = description.timestamp
        val prefs = getSharedPreferences("vitral", 0)
        if (rawStamp > 0L && prefs.getLong("clipboard_stamp", -1L) == rawStamp) return
        val text = clip.takeIf { it.itemCount > 0 }?.getItemAt(0)?.text?.toString() ?: return
        if (text.isEmpty()) return
        if (rawStamp <= 0L && text == lastPushedClipText) return
        val stamp = if (rawStamp > 0L) rawStamp else System.currentTimeMillis()
        lastPushedClipText = text
        ++clipboardRevision
        val revision = clipboardRevision
        if (rawStamp > 0L) prefs.edit().putLong("clipboard_stamp", rawStamp).apply()
        Core.call(
            this,
            "clipboard-push",
            json("payload" to json("content" to text, "timestamp_ms" to stamp)),
            background = true,
        ) {
            if (alive && revision == clipboardRevision) reconcileClipboard()
        }
    }

    private fun glassSheetDrawable(radiusDp: Int, topCornersOnly: Boolean): GradientDrawable {
        val dark = isDarkTheme
        val colors =
            if (dark) intArrayOf(0xEB192132.toInt(), 0xEE111723.toInt())
            else intArrayOf(0xF5F8FBFF.toInt(), 0xF2E9F0F8.toInt())
        return GradientDrawable(GradientDrawable.Orientation.TOP_BOTTOM, colors).apply {
            val r = dpF(radiusDp.toFloat())
            if (topCornersOnly) {
                cornerRadii = floatArrayOf(r, r, r, r, 0f, 0f, 0f, 0f)
            } else {
                cornerRadius = r
            }
            setStroke(
                dp(1),
                if (dark) 0x26FFFFFF else 0xD8FFFFFF.toInt(),
            )
        }
    }

    private fun pillDrawable(fillArgb: Long, radiusDp: Int, strokeArgb: Long? = null) =
        GradientDrawable().apply {
            setColor(fillArgb.toInt())
            cornerRadius = dpF(radiusDp.toFloat())
            if (strokeArgb != null) setStroke(dp(1), strokeArgb.toInt())
        }

    private fun switchSystemIme() {
        imm.showInputMethodPicker()
    }

    private fun layoutPanel() {
        if (!::frame.isInitialized) return
        val dm = resources.displayMetrics
        val win = window.window ?: return
        win.setBackgroundDrawable(ColorDrawable(Color.TRANSPARENT))
        val insets = win.decorView.rootWindowInsets
        val landscape = isLandscape
        val rootNav: Int
        val leftInset: Int
        val rightInset: Int
        if (Build.VERSION.SDK_INT >= 30) {
            val liveNav = insets?.getInsets(WindowInsets.Type.navigationBars())
            val liveTap = insets?.getInsets(WindowInsets.Type.tappableElement())
            val liveGesture = insets?.getInsets(WindowInsets.Type.mandatorySystemGestures())
            val liveCutout = insets?.getInsets(WindowInsets.Type.displayCutout())
            val maxInsets =
                getSystemService(WindowManager::class.java).maximumWindowMetrics.windowInsets
            val maxNav = maxInsets.getInsetsIgnoringVisibility(WindowInsets.Type.navigationBars())
            val maxCutout = maxInsets.getInsetsIgnoringVisibility(WindowInsets.Type.displayCutout())
            rootNav =
                maxOf(
                    imeBottomInsetPx,
                    liveNav?.bottom ?: 0,
                    liveTap?.bottom ?: 0,
                    liveGesture?.bottom ?: 0,
                    maxNav.bottom,
                )
            leftInset = maxOf(imeLeftInsetPx, liveNav?.left ?: 0, liveCutout?.left ?: 0, maxNav.left, maxCutout.left)
            rightInset =
                maxOf(imeRightInsetPx, liveNav?.right ?: 0, liveCutout?.right ?: 0, maxNav.right, maxCutout.right)
        } else {
            val cutout = insets?.displayCutout
            @Suppress("DEPRECATION")
            rootNav = maxOf(imeBottomInsetPx, insets?.stableInsetBottom ?: 0)
            @Suppress("DEPRECATION")
            leftInset = maxOf(imeLeftInsetPx, insets?.stableInsetLeft ?: 0, cutout?.safeInsetLeft ?: 0)
            @Suppress("DEPRECATION")
            rightInset = maxOf(imeRightInsetPx, insets?.stableInsetRight ?: 0, cutout?.safeInsetRight ?: 0)
        }
        // On Android 10–15 portrait gesture navigation, navigationBars().bottom may report only
        // ~20–24dp while SystemUI draws the ~48dp IME footer bar (globe + collapse chevron).
        // In landscape Android only draws the thin gesture pill, so keep the exact bottom inset.
        val nav =
            when {
                landscape -> rootNav
                rootNav > 0 -> rootNav.coerceAtLeast(dp(48))
                else -> 0
            }
        val usableHeight = (dm.heightPixels - nav).coerceAtLeast(1)
        val isFiveRow =
            mode == "keys" && !numbers && !flypy && T.Ergonomics.quanpinRows == 5
        val dock =
            dockedKeyboardHeight(
                screenHeightPx = dm.heightPixels,
                usableHeightPx = usableHeight,
                density = dm.density,
                isLandscape = landscape,
                isFiveRow = isFiveRow,
                heightPreset = heightPreset,
                dockedScreenRatio = T.Ergonomics.mobileDockedScreenRatio,
            )
        val bounds =
            panelBounds(
                dm.widthPixels,
                usableHeight,
                if (floating) floatWidth.takeIf { it > 0 } ?: (dm.widthPixels * 4 / 5)
                else dm.widthPixels,
                if (floating && floatHeight > 0) floatHeight else dock,
                if (floating) dp(T.Ergonomics.floatingMinWidthDp) else 0,
                if (floating) dp(T.Ergonomics.floatingMinHeightDp) else 0,
                if (floating) offsetX else 0,
                if (floating) offsetY else 0,
            )
        offsetX = bounds.left
        offsetY = bounds.bottom
        if (floating) {
            sheet.layoutParams = FrameLayout.LayoutParams(bounds.width, bounds.height)
            frame.minimumHeight = bounds.height
            win.attributes =
                win.attributes.apply {
                    gravity = Gravity.BOTTOM or Gravity.LEFT
                    x = bounds.left
                    y = bounds.bottom + nav
                    width = bounds.width
                    height = bounds.height
                    preferredRefreshRate = 120f
                }
            sheet.elevation = dpF(16f)
            sheet.background = glassSheetDrawable(T.Radius.mobileKeyboardSheetPx, topCornersOnly = false)
            sheet.setPadding(dp(7), dp(4), dp(7), dp(7))
        } else {
            val totalHeight = bounds.height + nav
            sheet.layoutParams =
                FrameLayout.LayoutParams(
                    FrameLayout.LayoutParams.MATCH_PARENT,
                    totalHeight,
                )
            frame.minimumHeight = totalHeight
            win.attributes =
                win.attributes.apply {
                    gravity = Gravity.BOTTOM or Gravity.LEFT
                    x = 0
                    y = 0
                    width = WindowManager.LayoutParams.MATCH_PARENT
                    height = WindowManager.LayoutParams.WRAP_CONTENT
                    preferredRefreshRate = 120f
                }
            sheet.elevation = 0f
            sheet.background = glassSheetDrawable(T.Radius.mobileKeyboardSheetPx, topCornersOnly = true)
            val (padLeft, padRight) =
                dockedHorizontalPadding(
                    screenWidthPx = dm.widthPixels,
                    leftInsetPx = leftInset,
                    rightInsetPx = rightInset,
                    density = dm.density,
                    isLandscape = landscape,
                )
            val padTop = dp(if (landscape) 3 else 5)
            val padBottom = dp(if (landscape) 3 else 6) + nav
            sheet.setPadding(padLeft, padTop, padRight, padBottom)
        }
        if (
            Build.VERSION.SDK_INT >= 31 &&
                getSystemService(WindowManager::class.java).isCrossWindowBlurEnabled
        ) {
            win.setBackgroundBlurRadius(
                dp(if (floating) T.Blur.backdropFloatingWindowPx else T.Blur.backdropStandardPx)
            )
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

    private fun label(
        text: String,
        highlight: Boolean = false,
        solidAccent: Boolean = false,
        subtle: Boolean = false,
        mono: Boolean = false,
        click: () -> Unit,
    ): TextView {
        val dark = isDarkTheme
        return TextView(this).apply {
            this.text = text
            textSize =
                when {
                    mono -> 12f
                    subtle -> (font - 2.5f).coerceAtLeast(11.5f)
                    else -> font
                }
            typeface =
                when {
                    mono -> Typeface.MONOSPACE
                    solidAccent || highlight -> boldTypeface
                    else -> mediumTypeface
                }
            setTextColor(
                when {
                    solidAccent -> 0xFFFFFFFFL
                    highlight && dark -> T.Dark.accentTextArgb
                    highlight -> T.Light.accentTextArgb
                    subtle && dark -> T.Dark.textSecondaryArgb
                    subtle -> T.Light.textSecondaryArgb
                    dark -> T.Dark.textPrimaryArgb
                    else -> T.Light.textPrimaryArgb
                }.toInt()
            )
            gravity = Gravity.CENTER
            setPadding(dp(if (mono) 8 else 12), dp(3), dp(if (mono) 8 else 12), dp(3))
            background =
                when {
                    solidAccent ->
                        pillDrawable(
                            if (dark) T.Dark.accentPrimaryArgb else T.Light.accentPrimaryArgb,
                            T.Radius.candidateActivePillPx,
                        )
                    highlight ->
                        pillDrawable(
                            if (dark) T.Dark.accentSoftArgb else T.Light.accentSoftArgb,
                            T.Radius.candidateActivePillPx,
                            if (dark) 0x450EA5E9L else 0x350284C7L,
                        )
                    mono ->
                        pillDrawable(
                            if (dark) 0x16FFFFFFL else 0x0E0F172AL,
                            8,
                            if (dark) 0x1EFFFFFFL else 0x140F172AL,
                        )
                    else -> null
                }
            isFocusable = true
            setOnClickListener { click() }
        }
    }

    private fun icon(
        parent: LinearLayout,
        id: Int,
        desc: String,
        activeState: Boolean = false,
        accentTint: Boolean = false,
        weight: Float = 0f,
        longClick: (() -> Unit)? = null,
        click: () -> Unit,
    ): ImageButton {
        val dark = isDarkTheme
        val button = ImageButton(this)
        button.setImageResource(id)
        button.scaleType = ImageView.ScaleType.CENTER_INSIDE
        button.setColorFilter(
            when {
                (activeState || accentTint) && dark -> T.Dark.accentTextArgb
                activeState || accentTint -> T.Light.accentTextArgb
                dark -> T.Dark.textPrimaryArgb
                else -> T.Light.textPrimaryArgb
            }.toInt()
        )
        button.alpha = if (activeState || accentTint) 1f else 0.78f
        button.contentDescription = desc
        button.background =
            if (activeState)
                pillDrawable(
                    if (dark) T.Dark.accentSoftArgb else T.Light.accentSoftArgb,
                    10,
                    if (dark) 0x3D0EA5E9L else 0x300284C7L,
                )
            else null
        button.setOnClickListener { click() }
        if (longClick != null) {
            button.setOnLongClickListener {
                longClick()
                true
            }
        }
        val compactChrome = isLandscape && !floating
        button.setPadding(dp(9), dp(if (compactChrome) 4 else 6), dp(9), dp(if (compactChrome) 4 else 6))
        val iconHeight = dp(if (compactChrome) 26 else 32)
        val lp =
            if (weight > 0f) {
                LinearLayout.LayoutParams(0, iconHeight, weight).apply {
                    setMargins(dp(2), dp(1), dp(2), dp(1))
                }
            } else {
                LinearLayout.LayoutParams(dp(40), iconHeight).apply {
                    setMargins(dp(2), dp(1), dp(2), dp(1))
                }
            }
        parent.addView(button, lp)
        return button
    }

    private fun langBadge(parent: LinearLayout, weight: Float = 0f, click: () -> Unit) {
        val dark = isDarkTheme
        val badge =
            TextView(this).apply {
                text = if (english) "En" else "中"
                textSize = 12.5f
                typeface = Typeface.create("sans-serif-medium", Typeface.BOLD)
                gravity = Gravity.CENTER
                contentDescription = if (english) "英文，切换为中文" else "中文，切换为英文"
                setTextColor(
                    when {
                        !english && dark -> T.Dark.accentTextArgb
                        !english -> T.Light.accentTextArgb
                        dark -> T.Dark.textPrimaryArgb
                        else -> T.Light.textPrimaryArgb
                    }.toInt()
                )
                background =
                    pillDrawable(
                        when {
                            !english && dark -> T.Dark.accentSoftArgb
                            !english -> T.Light.accentSoftArgb
                            dark -> 0x12FFFFFFL
                            else -> 0x0E0F172AL
                        },
                        10,
                        if (!english) (if (dark) 0x3D0EA5E9L else 0x300284C7L) else null,
                    )
                setOnClickListener { click() }
                setOnLongClickListener {
                    switchSystemIme()
                    true
                }
            }
        val compactChrome = isLandscape && !floating
        val badgeHeight = dp(if (compactChrome) 24 else 28)
        val lp =
            if (weight > 0f) {
                LinearLayout.LayoutParams(0, badgeHeight, weight).apply {
                    setMargins(dp(3), dp(if (compactChrome) 1 else 2), dp(3), dp(if (compactChrome) 1 else 2))
                }
            } else {
                LinearLayout.LayoutParams(dp(38), badgeHeight).apply {
                    setMargins(dp(3), dp(if (compactChrome) 1 else 2), dp(3), dp(if (compactChrome) 1 else 2))
                }
            }
        parent.addView(badge, lp)
    }

    private fun drag(view: View, resize: Boolean) {
        var x = 0f
        var y = 0f
        var w = 0
        var h = 0
        var left = 0
        var bottom = 0
        view.setOnTouchListener { _, e ->
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
                MotionEvent.ACTION_UP -> view.performClick()
            }
            true
        }
    }

    private fun keysTreeSignature(dark: Boolean): String {
        val aeviouEnvTag = if (!flypy && aeviouExperimental) effectiveAeviouEnv().name else ""
        return "keys|$floating|$isLandscape|$dark|$flypy|$english|$numbers|$aeviouExperimental|$aeviouLeftHanded|$aeviouEnvTag|$fieldKind|$enterLabel|$sublabelMode|$font"
    }

    private fun render() {
        if (!::sheet.isInitialized) return
        if (pressedPointers.isNotEmpty()) {
            renderPending = true
            return
        }
        renderPending = false
        val dark = isDarkTheme
        val nextKeysSig = if (mode == "keys") keysTreeSignature(dark) else null
        if (nextKeysSig != null && renderedTreeSignature == nextKeysSig && sheet.childCount > 0) {
            refreshKeyboardCase()
            candidates()
            if (aeviouView != null) refreshAeviouContextHints()
            return
        }
        renderedTreeSignature = nextKeysSig
        idleStripSignature = null
        composingStripActive = false
        composingStripDark = null
        stripCandidateSlots.clear()
        lastKeyboardCaseComposing = null
        lastKeyboardCaseEnglish = null
        lastKeyboardCaseShift = -1
        aeviouView = null
        sheet.removeAllViews()

        // Floating mode top drag pill
        if (floating) {
            val dragBar =
                LinearLayout(this).apply {
                    gravity = Gravity.CENTER
                    setPadding(dp(8), dp(2), dp(8), dp(4))
                }
            val pill =
                View(this).apply {
                    background = pillDrawable(if (dark) 0x45FFFFFFL else 0x300F172AL, 99)
                }
            dragBar.addView(pill, LinearLayout.LayoutParams(dp(40), dp(4)))
            drag(dragBar, false)
            sheet.addView(dragBar, LinearLayout.LayoutParams(-1, dp(14)))
        }

        // Row 1: Airy Pure-Vector Icon Toolbar
        val compactChrome = isLandscape && !floating
        val toolbar =
            LinearLayout(this).apply {
                gravity = Gravity.CENTER_VERTICAL
                setPadding(dp(2), 0, dp(2), dp(if (compactChrome) 1 else 2))
            }
        sheet.addView(toolbar, LinearLayout.LayoutParams(-1, dp(if (compactChrome) 30 else 36)))

        icon(
            toolbar,
            R.drawable.tools,
            "工具面板",
            activeState = mode == "tools",
            accentTint = true,
            weight = 1f,
        ) {
            switch(if (mode == "tools") "keys" else "tools")
        }
        langBadge(toolbar, weight = 0.9f) {
            manualLangOverride = true
            processKeyDirect(0, MOD_LANG_TOGGLE) {
                applyAction(it)
            }
        }
        icon(toolbar, R.drawable.write, "手写", activeState = mode == "continuous" || mode == "lookup", weight = 1f) {
            switch(if (mode == "continuous") "keys" else "continuous")
        }
        icon(
            toolbar,
            R.drawable.clipboard,
            "剪贴板",
            activeState = mode == "clipboard" || mode == "clipboard_shortcut",
            weight = 1f,
        ) {
            when (mode) {
                "clipboard_shortcut" -> returnToClipboardFromShortcutEditor()
                "clipboard" -> switch("keys")
                else -> switch("clipboard")
            }
        }
        if (!flypy && mode == "keys") {
            icon(
                toolbar,
                R.drawable.honeycomb,
                "实验性 AEVIOU 蜂窝键盘",
                activeState = aeviouExperimental,
                accentTint = aeviouExperimental,
                weight = 1f,
            ) {
                aeviouExperimental = !aeviouExperimental
                val cfg = Core.settings(this).put("mobile_experimental_aeviou", aeviouExperimental)
                Core.save(this, cfg) {}
                layoutPanel()
                render()
            }
        }
        icon(toolbar, R.drawable.window, "悬浮或停靠", activeState = floating, weight = 1f) {
            floating = !floating
            layoutPanel()
            render()
        }
        icon(toolbar, R.drawable.collapse, "收起键盘", weight = 0.9f) {
            requestHideSelf(0)
        }

        // Hairline glass divider
        sheet.addView(
            View(this).apply {
                setBackgroundColor(if (dark) 0x16FFFFFF else 0x120F172A)
            },
            -1,
            dp(1),
        )

        // Row 2: Smart Candidate Strip (never an empty black trench!)
        val scroll = HorizontalScrollView(this).apply { isHorizontalScrollBarEnabled = false }
        bar =
            LinearLayout(this).apply {
                gravity = Gravity.CENTER_VERTICAL
                setPadding(dp(4), dp(if (compactChrome) 2 else 3), dp(4), dp(if (compactChrome) 2 else 3))
            }
        scroll.addView(bar)
        sheet.addView(scroll, -1, dp(if (compactChrome) 34 else 40))
        candidates()

        // Row 3: Active Input Surface
        val bodyWrapper = FrameLayout(this)
        body = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        bodyWrapper.addView(body, FrameLayout.LayoutParams(-1, -1))
        if (floating) {
            val grip =
                ImageView(this).apply {
                    setImageResource(R.drawable.resize)
                    setColorFilter(
                        (if (dark) T.Dark.accentTextArgb else T.Light.accentPrimaryArgb).toInt()
                    )
                    alpha = 0.72f
                    setPadding(dp(5), dp(5), dp(3), dp(3))
                    contentDescription = "拖动缩放小窗"
                }
            drag(grip, true)
            bodyWrapper.addView(
                grip,
                FrameLayout.LayoutParams(dp(24), dp(24), Gravity.BOTTOM or Gravity.END),
            )
        }
        sheet.addView(bodyWrapper, LinearLayout.LayoutParams(-1, 0, 1f))

        when (mode) {
            "tools" -> showToolsSheet()
            "candidates" -> showCandidateGrid()
            "emoji" -> showEmojiPanel()
            "symbols" -> showSymbolPanel()
            "edit" -> showTextEditPanel()
            "lookup",
            "continuous" -> {
                if (hwModelReady == false) {
                    showHwMissingPrompt()
                } else {
                    val gen = canvasGeneration
                    body.addView(
                        InkView(this, mode == "continuous", darkOverride = dark) { xs, ys, times ->
                            Core.run(
                                this,
                                operation = {
                                    unpack(VitralNative.hwPushStroke(it, xs, ys, times, true))
                                },
                            ) { r ->
                                if (gen == canvasGeneration && active)
                                    r.onSuccess {
                                            hwErrorMessage = ""
                                            showInk(it as JSONObject)
                                        }
                                        .onFailure { err ->
                                            val msg = err.message.orEmpty()
                                            hwErrorMessage = msg
                                            if (
                                                "手写" in msg ||
                                                    "模型" in msg ||
                                                    "ONNX" in msg ||
                                                    "hw_model" in msg
                                            ) {
                                                hwModelReady = false
                                                render()
                                            } else {
                                                candidates()
                                                message(msg)
                                            }
                                        }
                            }
                        },
                        -1,
                        -1,
                    )
                }
            }
            "clipboard" -> {
                val cached = clipboardItems
                if (cached != null) showRing(cached)
                val fetchSeq = clipboardMutationSeq
                call("clipboard-state") {
                    if (mode == "clipboard" && fetchSeq == clipboardMutationSeq) {
                        val parsed = parseClipboardRing(it)
                        val changed = parsed != clipboardItems
                        clipboardItems = parsed
                        if (changed || cached == null) showRing(parsed)
                    }
                }
            }
            "clipboard_shortcut" -> showShortcutCodeEditor()
            else -> keyboard()
        }
    }

    private fun openSettingsTab(tabIndex: Int = 0, modelGroup: String? = null, modelCap: String? = null) {
        startActivity(
            Intent(this, SettingsActivity::class.java)
                .putExtra("tab", tabIndex)
                .apply {
                    if (modelGroup != null) putExtra("model_group", modelGroup)
                    if (modelCap != null) putExtra("model_cap", modelCap)
                }
                .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_SINGLE_TOP)
        )
    }

    private fun showHwMissingPrompt() {
        val dark = isDarkTheme
        body.removeAllViews()
        val wrapper =
            LinearLayout(this).apply {
                orientation = LinearLayout.VERTICAL
                gravity = Gravity.CENTER
                setPadding(dp(20), dp(14), dp(20), dp(14))
            }
        val card =
            LinearLayout(this).apply {
                orientation = LinearLayout.VERTICAL
                gravity = Gravity.CENTER_HORIZONTAL
                setPadding(dp(18), dp(16), dp(18), dp(16))
                background =
                    pillDrawable(
                        if (dark) 0x16FFFFFFL else 0xEAFFFFFFL,
                        16,
                        if (dark) 0x350EA5E9L else 0x2E0284C7L,
                    )
            }
        card.addView(
            TextView(this).apply {
                text = "尚未下载离线手写模型"
                textSize = 15f
                typeface = Typeface.create("sans-serif-medium", Typeface.BOLD)
                setTextColor((if (dark) T.Dark.textPrimaryArgb else T.Light.textPrimaryArgb).toInt())
                gravity = Gravity.CENTER
            }
        )
        card.addView(
            TextView(this).apply {
                text =
                    hwErrorMessage.takeIf { it.isNotBlank() && it.length <= 60 }
                        ?: "下载「离线手写识别模型」(约 16 MB) 并启用后，即可直接手写认字与连写联句。"
                textSize = 12.5f
                setTextColor(
                    (if (dark) T.Dark.textSecondaryArgb else T.Light.textSecondaryArgb).toInt()
                )
                gravity = Gravity.CENTER
                setPadding(0, dp(6), 0, dp(12))
            }
        )
        val btnRow =
            LinearLayout(this).apply {
                orientation = LinearLayout.HORIZONTAL
                gravity = Gravity.CENTER
            }
        btnRow.addView(
            label("前往模型中心下载", solidAccent = true) { openSettingsTab(2, "multimodal", "handwriting") }.apply {
                setPadding(dp(16), dp(8), dp(16), dp(8))
            }
        )
        btnRow.addView(
            label("返回键盘", subtle = true) { switch("keys") }.apply {
                setPadding(dp(12), dp(8), dp(12), dp(8))
            },
            LinearLayout.LayoutParams(-2, -2).apply { setMargins(dp(10), 0, 0, 0) },
        )
        card.addView(btnRow)
        wrapper.addView(card, LinearLayout.LayoutParams(-1, -2))
        body.addView(wrapper, LinearLayout.LayoutParams(-1, -1))
    }

    /** Sleek Liquid Glass in-keyboard Quick Tools Sheet (replaces cramped secondary icon strip). */
    private fun showToolsSheet() {
        val dark = isDarkTheme
        body.removeAllViews()
        val container =
            LinearLayout(this).apply {
                orientation = LinearLayout.VERTICAL
                setPadding(dp(6), dp(8), dp(6), dp(6))
            }
        body.addView(container, LinearLayout.LayoutParams(-1, -1))

        data class ToolTile(val iconId: Int, val title: String, val active: Boolean = false, val onTap: () -> Unit)
        val tiles =
            listOf(
                ToolTile(R.drawable.pen, "单字查音", mode == "lookup") { switch("lookup") },
                ToolTile(R.drawable.write, "连续手写", mode == "continuous") { switch("continuous") },
                ToolTile(R.drawable.clipboard, "剪贴板") { switch("clipboard") },
                ToolTile(R.drawable.resize, "文本编辑", mode == "edit") {
                    editSelectMode = false
                    switch("edit")
                },
                ToolTile(R.drawable.translate, "翻译选区") {
                    mode = "keys"
                    render()
                    inlineAi("translate")
                },
                ToolTile(R.drawable.polish, "润色选区") {
                    mode = "keys"
                    render()
                    inlineAi("polish")
                },
                ToolTile(R.drawable.tools, "表情符号", mode == "emoji") { switch("emoji") },
                ToolTile(R.drawable.settings, "全部设置") { openSettingsTab(0) },
            )

        for (rowIdx in 0 until 2) {
            val row = LinearLayout(this)
            container.addView(row, LinearLayout.LayoutParams(-1, 0, 1f))
            for (colIdx in 0 until 4) {
                val tile = tiles[rowIdx * 4 + colIdx]
                val card =
                    LinearLayout(this).apply {
                        orientation = LinearLayout.VERTICAL
                        gravity = Gravity.CENTER
                        setPadding(dp(6), dp(8), dp(6), dp(8))
                        background =
                            pillDrawable(
                                when {
                                    tile.active && dark -> T.Dark.accentSoftArgb
                                    tile.active -> T.Light.accentSoftArgb
                                    dark -> 0x14FFFFFFL
                                    else -> 0xE6FFFFFFL
                                },
                                14,
                                if (tile.active) (if (dark) 0x450EA5E9L else 0x350284C7L)
                                else (if (dark) 0x1AFFFFFFL else 0x140F172AL),
                            )
                        setOnClickListener { tile.onTap() }
                    }
                val iv =
                    ImageView(this).apply {
                        setImageResource(tile.iconId)
                        setColorFilter(
                            (if (tile.active) {
                                    if (dark) T.Dark.accentTextArgb else T.Light.accentPrimaryArgb
                                } else {
                                    if (dark) T.Dark.textPrimaryArgb else T.Light.textPrimaryArgb
                                })
                                .toInt()
                        )
                    }
                card.addView(iv, LinearLayout.LayoutParams(dp(22), dp(22)))
                card.addView(
                    TextView(this).apply {
                        text = tile.title
                        textSize = 11.5f
                        typeface = Typeface.create("sans-serif-medium", Typeface.NORMAL)
                        setPadding(0, dp(6), 0, 0)
                        setTextColor(
                            (if (tile.active) {
                                    if (dark) T.Dark.accentTextArgb else T.Light.accentTextArgb
                                } else {
                                    if (dark) T.Dark.textPrimaryArgb else T.Light.textPrimaryArgb
                                })
                                .toInt()
                        )
                    }
                )
                row.addView(
                    card,
                    LinearLayout.LayoutParams(0, -1, 1f).apply {
                        setMargins(dp(4), dp(4), dp(4), dp(4))
                    },
                )
            }
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
        idleStripSignature = null
        composingStripActive = false
        bar.removeAllViews()
        addBarItem(
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
            auxiliary = true,
        ) { response ->
            if (!active || generation != editor || request != inlineRequest) return@call
            if (currentInputConnection?.getSelectedText(0)?.toString() != original) {
                candidates()
                return@call
            }
            response
                .onSuccess {
                    val replacement = (it as JSONObject).getString("output_text")
                    idleStripSignature = null
                    composingStripActive = false
                    bar.removeAllViews()
                    addBarItem(
                        label("点击替换：$replacement", solidAccent = true) {
                            if (currentInputConnection?.getSelectedText(0)?.toString() == original) {
                                currentInputConnection?.commitText(replacement, 1)
                                invalidateSurroundingCache()
                            }
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
        inputGeneration++
        backspaceFlight.reset()
        cancelPrediction()
        inlineRequest++
        canvasGeneration++
        if (next != "clipboard_shortcut") clearShortcutEditorState()
        if (next != "clipboard") {
            clipboardScrollView = null
            if (next != "clipboard_shortcut") clipboardScrollY = 0
        }
        mode = next
        shown = JSONArray()
        lastInkResult = null
        hasPreviousPage = false
        hasNextPage = false
        if (composing || hwComposing) currentInputConnection?.setComposingText("", 1)
        composing = false
        hwComposing = false
        preeditText = ""
        preeditCursorUtf8 = 0
        Core.run(this, operation = { unpack(VitralNative.processKey(it, 27, 0, 0)) })
        if (next == "lookup" || next == "continuous") {
            hwErrorMessage = ""
            hwModelReady = null
            call(
                "hw-begin",
                json("mode" to if (next == "lookup") "PhoneticLookup" else "ContinuousLattice"),
            )
            val gen = canvasGeneration
            Core.call(this, "models-state", background = true) { r ->
                if (!active || gen != canvasGeneration || (mode != "lookup" && mode != "continuous"))
                    return@call
                r.onSuccess { value ->
                    val arr = (value as? JSONObject)?.optJSONArray("models")
                    var ready = false
                    if (arr != null) {
                        for (i in 0 until arr.length()) {
                            val m = arr.optJSONObject(i) ?: continue
                            val cap = m.optString("capability")
                            if (cap == "handwriting" && m.optBoolean("active")) {
                                ready = true
                                break
                            }
                        }
                    }
                    hwModelReady = ready
                    if (!ready) render()
                }
            }
        }
        layoutPanel()
        render()
        if (next == "keys") refreshCandidates()
    }

    private fun addBarItem(v: View, marginHorizontalDp: Int = 2) {
        val lp =
            LinearLayout.LayoutParams(
                    LinearLayout.LayoutParams.WRAP_CONTENT,
                    LinearLayout.LayoutParams.MATCH_PARENT,
                )
                .apply { setMargins(dp(marginHorizontalDp), 0, dp(marginHorizontalDp), 0) }
        bar.addView(v, lp)
    }

    private fun applySlotHighlight(slot: CandidateSlot, highlight: Boolean, dark: Boolean) {
        slot.highlighted = highlight
        val v = slot.view
        v.typeface = if (highlight) boldTypeface else mediumTypeface
        v.setTextColor(
            (when {
                    highlight && dark -> T.Dark.accentTextArgb
                    highlight -> T.Light.accentTextArgb
                    dark -> T.Dark.textPrimaryArgb
                    else -> T.Light.textPrimaryArgb
                })
                .toInt()
        )
        v.background =
            if (highlight) {
                slot.highlightBg
                    ?: pillDrawable(
                            if (dark) T.Dark.accentSoftArgb else T.Light.accentSoftArgb,
                            T.Radius.candidateActivePillPx,
                            if (dark) 0x450EA5E9L else 0x350284C7L,
                        )
                        .also { slot.highlightBg = it }
            } else {
                null
            }
    }

    private fun updateComposingStrip() {
        val dark = isDarkTheme
        if (!composingStripActive || composingStripDark != dark || composingStripFont != font) {
            bar.removeAllViews()
            stripCandidateSlots.clear()
            composingStripActive = true
            composingStripDark = dark
            composingStripFont = font
            stripSplitActive = null

            val preeditView = label("", mono = true) {}
            bindEditablePreeditTap(preeditView)
            stripPreeditView = preeditView
            addBarItem(preeditView, marginHorizontalDp = 3)

            val delimiterView =
                label("分词'", subtle = true) {
                    if (canInsertDelimiterAtCursor()) {
                        processKeyDirect('\''.code) { applyAction(it) }
                    }
                }.apply {
                    setPadding(dp(8), dp(2), dp(8), dp(2))
                    contentDescription = "插入拼音分词符"
                }
            stripDelimiterView = delimiterView
            addBarItem(delimiterView, marginHorizontalDp = 2)

            val splitView =
                label("拆音", subtle = true) {
                    syllableSplit = !syllableSplit
                    call("set-syllable-split", json("enabled" to syllableSplit)) {
                        applyAction(it)
                    }
                }.apply { setPadding(dp(8), dp(2), dp(8), dp(2)) }
            stripSplitView = splitView
            addBarItem(splitView, marginHorizontalDp = 2)

            val container =
                LinearLayout(this).apply {
                    orientation = LinearLayout.HORIZONTAL
                    gravity = Gravity.CENTER_VERTICAL
                }
            stripCandidatesContainer = container
            bar.addView(
                container,
                LinearLayout.LayoutParams(
                    LinearLayout.LayoutParams.WRAP_CONTENT,
                    LinearLayout.LayoutParams.MATCH_PARENT,
                ),
            )

            val prevView = label("‹", subtle = true) { processKeyDirect(0xff55) { applyAction(it) } }
            stripPrevView = prevView
            addBarItem(prevView)

            val nextView = label("›", subtle = true) { processKeyDirect(0xff56) { applyAction(it) } }
            stripNextView = nextView
            addBarItem(nextView)

            val expandView =
                label("⌄", highlight = true) {
                    mode = "candidates"
                    render()
                }.apply { contentDescription = "展开候选词" }
            stripExpandView = expandView
            addBarItem(expandView)
        }

        (bar.parent as? HorizontalScrollView)?.scrollTo(0, 0)

        val preeditView = stripPreeditView
        if (preeditView != null) {
            if (preeditText.isNotEmpty()) {
                preeditView.text = styledPreeditText()
                preeditView.background = preeditPillBackground(dark)
                preeditView.visibility = View.VISIBLE
            } else {
                preeditView.visibility = View.GONE
            }
        }

        val delimiterView = stripDelimiterView
        if (delimiterView != null) {
            delimiterView.visibility =
                if (preeditText.isNotEmpty() && !english) View.VISIBLE else View.GONE
        }

        val splitView = stripSplitView
        if (splitView != null) {
            val showSplit = preeditText.isNotEmpty() && !flypy && !english
            if (showSplit) {
                if (stripSplitActive != syllableSplit) {
                    stripSplitActive = syllableSplit
                    splitView.typeface = if (syllableSplit) boldTypeface else mediumTypeface
                    splitView.textSize =
                        if (syllableSplit) font else (font - 2.5f).coerceAtLeast(11.5f)
                    splitView.setTextColor(
                        (when {
                                syllableSplit -> 0xFFFFFFFFL
                                dark -> T.Dark.textSecondaryArgb
                                else -> T.Light.textSecondaryArgb
                            })
                            .toInt()
                    )
                    splitView.background =
                        if (syllableSplit)
                            pillDrawable(
                                if (dark) T.Dark.accentPrimaryArgb else T.Light.accentPrimaryArgb,
                                T.Radius.candidateActivePillPx,
                            )
                        else null
                    splitView.contentDescription =
                        if (syllableSplit) "恢复整音节连词" else "拆分当前音节"
                }
                splitView.visibility = View.VISIBLE
            } else {
                splitView.visibility = View.GONE
            }
        }

        val container = stripCandidatesContainer ?: return
        val sheetW = sheet.width.takeIf { it > 0 } ?: resources.displayMetrics.widthPixels
        val normalMaxW = (sheetW / 3).coerceAtLeast(dp(56))
        val quickPasteMaxW = sheetW * 3 / 4
        val count = shown.length()

        while (stripCandidateSlots.size < count) {
            val view =
                label("") {}.apply {
                    setSingleLine(true)
                    ellipsize = android.text.TextUtils.TruncateAt.END
                    maxWidth = normalMaxW
                }
            val slot = CandidateSlot(view)
            view.setOnClickListener {
                selectCandidateDirect(slot.index) { applyAction(it) }
            }
            view.setOnLongClickListener { anchor ->
                if (!slot.canMenu) return@setOnLongClickListener false
                val targetIndex = slot.index
                PopupMenu(this, anchor).apply {
                    menu.add("置顶此词").setOnMenuItemClickListener {
                        pinCandidateDirect(targetIndex) {
                            applyAction(it)
                            if (mode == "candidates") showCandidateGrid()
                        }
                        true
                    }
                    menu.add("移除此词").setOnMenuItemClickListener {
                        eraseCandidateDirect(targetIndex) {
                            applyAction(it)
                            if (mode == "candidates") showCandidateGrid()
                        }
                        true
                    }
                }.show()
                true
            }
            stripCandidateSlots.add(slot)
            val lp =
                LinearLayout.LayoutParams(
                        LinearLayout.LayoutParams.WRAP_CONTENT,
                        LinearLayout.LayoutParams.MATCH_PARENT,
                    )
                    .apply { setMargins(dp(2), 0, dp(2), 0) }
            container.addView(view, lp)
        }

        for (i in 0 until count) {
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
            val highlight = (i == 0) || isQuickPaste || isFunc
            val slot = stripCandidateSlots[i]
            slot.index = item.getInt("index")
            slot.canMenu = !isQuickPaste && !isFunc
            val v = slot.view
            if (v.text != display) v.text = display
            val targetMaxW = if (isQuickPaste) quickPasteMaxW else normalMaxW
            if (v.maxWidth != targetMaxW) v.maxWidth = targetMaxW
            slot.quickPaste = isQuickPaste
            if (slot.highlighted != highlight) {
                applySlotHighlight(slot, highlight, dark)
            }
            v.contentDescription =
                (if (isQuickPaste) "快捷粘贴：" else "") + item.getString("text")
            if (v.visibility != View.VISIBLE) v.visibility = View.VISIBLE
        }
        for (i in count until stripCandidateSlots.size) {
            val v = stripCandidateSlots[i].view
            if (v.visibility != View.GONE) v.visibility = View.GONE
        }

        stripPrevView?.visibility = if (hasPreviousPage) View.VISIBLE else View.GONE
        stripNextView?.visibility = if (hasNextPage) View.VISIBLE else View.GONE
        stripExpandView?.visibility =
            if (count >= 4 || hasNextPage || hasPreviousPage) View.VISIBLE else View.GONE
    }

    private fun candidates() {
        if (!::bar.isInitialized) return
        if (mode == "keys" && composing) {
            idleStripSignature = null
            updateComposingStrip()
            return
        }
        composingStripActive = false
        val idleQuickPunc =
            mode == "keys" &&
                !composing &&
                !inlineSuggestionsActive &&
                shown.length() == 0 &&
                predictionText.isEmpty() &&
                lastDeletedText.isEmpty()
        if (idleQuickPunc) {
            val sig = "idle:$fieldKind:$english:$isDarkTheme:$font"
            if (idleStripSignature == sig && bar.childCount > 0) return
            idleStripSignature = sig
        } else {
            idleStripSignature = null
        }
        bar.removeAllViews()
        (bar.parent as? HorizontalScrollView)?.scrollTo(0, 0)
        if (mode == "tools") {
            addBarItem(label("← 返回键盘", highlight = true) { switch("keys") })
            addBarItem(
                label(
                    when (heightPreset) {
                        "compact" -> "高度：紧凑"
                        "comfort" -> "高度：舒适"
                        else -> "高度：标准"
                    },
                    subtle = true,
                ) {
                    heightPreset =
                        when (heightPreset) {
                            "compact" -> "standard"
                            "standard" -> "comfort"
                            else -> "compact"
                        }
                    val cfg = Core.settings(this).put("mobile_keyboard_height", heightPreset)
                    Core.save(this, cfg) {}
                    layoutPanel()
                    candidates()
                }
            )
            return
        }
        if (mode == "lookup" || mode == "continuous") {
            addBarItem(label("← 键盘", subtle = true) { switch("keys") })
            if (hwModelReady == false) {
                addBarItem(label("下载离线手写模型", solidAccent = true) { openSettingsTab(2, "multimodal", "handwriting") })
                return
            }
            addBarItem(label("清空", subtle = true) { switch(mode) })
            if (mode == "continuous")
                addBarItem(
                    label("确认上屏", solidAccent = true) {
                        val gen = canvasGeneration
                        call(
                            "hw-end",
                            onSameEditorPanelSwitched = { r ->
                                val text = r.optString("composing_text")
                                if (text.isNotEmpty()) {
                                    currentInputConnection?.commitText(text, 1)
                                    invalidateSurroundingCache()
                                    if (!passwordEditor) lastCommittedText = text
                                }
                            },
                        ) { r ->
                            val text = r.optString("composing_text")
                            if (text.isNotEmpty()) {
                                currentInputConnection?.commitText(text, 1)
                                invalidateSurroundingCache()
                                if (!passwordEditor) lastCommittedText = text
                            }
                            hwComposing = false
                            if (gen == canvasGeneration) switch(mode)
                        }
                    }
                )
            return
        }
        if (mode == "clipboard_shortcut") {
            addBarItem(label("← 返回", highlight = true) { returnToClipboardFromShortcutEditor() })
            val preview = editingShortcutContent.replace(Regex("\\s+"), " ").trim()
            if (preview.isNotEmpty()) {
                addBarItem(
                    label("收藏：$preview", subtle = true) {}.apply {
                        setSingleLine(true)
                        maxWidth =
                            ((sheet.width.takeIf { it > 0 } ?: resources.displayMetrics.widthPixels) * 2 / 3)
                                .coerceAtLeast(dp(120))
                        ellipsize = android.text.TextUtils.TruncateAt.END
                        isClickable = false
                        isFocusable = false
                    }
                )
            }
            return
        }
        if (mode == "clipboard" || mode == "emoji" || mode == "symbols" || mode == "edit") {
            addBarItem(label("← 返回键盘", highlight = true) { switch("keys") })
            if (mode == "edit") {
                addBarItem(
                    label(if (editSelectMode) "选择中" else "开启选择", solidAccent = editSelectMode, subtle = !editSelectMode) {
                        editSelectMode = !editSelectMode
                        candidates()
                        showTextEditPanel()
                    }
                )
            }
            if (mode == "emoji") {
                val cats = listOf("常用", "表情", "手势", "符号")
                cats.forEachIndexed { idx, title ->
                    addBarItem(
                        label(title, highlight = emojiCategory == idx, subtle = emojiCategory != idx) {
                            emojiCategory = idx
                            render()
                        }
                    )
                }
            }
            if (mode == "symbols") {
                val cats = listOf("最近", "中文", "英文", "数学", "序号", "特殊")
                cats.forEachIndexed { idx, title ->
                    addBarItem(
                        label(title, highlight = symbolCategory == idx, subtle = symbolCategory != idx) {
                            symbolCategory = idx
                            render()
                        }
                    )
                }
                addBarItem(label("☺", subtle = true) { switch("emoji") })
            }
            return
        }
        if (mode == "candidates") {
            if (preeditText.isNotEmpty()) {
                val preView = label("", mono = true) {}.apply {
                    text = styledPreeditText()
                    background = preeditPillBackground(isDarkTheme)
                }
                bindEditablePreeditTap(preView)
                addBarItem(preView, marginHorizontalDp = 3)
            }
            addBarItem(
                label("⌃ 收起", highlight = true) {
                    mode = "keys"
                    render()
                }
            )
            if (hasPreviousPage)
                addBarItem(
                    label("‹ 上页", subtle = true) {
                        processKeyDirect(0xff55) {
                            applyAction(it)
                            if (mode == "candidates") showCandidateGrid()
                        }
                    }
                )
            if (hasNextPage)
                addBarItem(
                    label("下页 ›", subtle = true) {
                        processKeyDirect(0xff56) {
                            applyAction(it)
                            if (mode == "candidates") showCandidateGrid()
                        }
                    }
                )
            return
        }
        if (composing && preeditText.isNotEmpty()) {
            val preView = label("", mono = true) {}.apply {
                text = styledPreeditText()
                background = preeditPillBackground(isDarkTheme)
            }
            bindEditablePreeditTap(preView)
            addBarItem(preView, marginHorizontalDp = 3)
            if (!english) {
                addBarItem(
                    label("分词'", subtle = true) {
                        if (canInsertDelimiterAtCursor()) {
                            processKeyDirect('\''.code) { applyAction(it) }
                        }
                    }.apply {
                        setPadding(dp(8), dp(2), dp(8), dp(2))
                        contentDescription = "插入拼音分词符"
                    },
                    marginHorizontalDp = 2,
                )
            }
            if (!flypy && !english) {
                addBarItem(
                    label("拆音", solidAccent = syllableSplit, subtle = !syllableSplit) {
                        syllableSplit = !syllableSplit
                        call("set-syllable-split", json("enabled" to syllableSplit)) {
                            applyAction(it)
                        }
                    }.apply {
                        setPadding(dp(8), dp(2), dp(8), dp(2))
                        contentDescription = if (syllableSplit) "恢复整音节连词" else "拆分当前音节"
                    },
                    marginHorizontalDp = 2,
                )
            }
        }
        if (!composing && mode == "keys" && inlineSuggestionsActive) {
            val inlineViews = inlineSuggestionViews + inlinePinnedViews
            inlineViews.forEach { v ->
                (v.parent as? ViewGroup)?.removeView(v)
                val lp =
                    LinearLayout.LayoutParams(
                            LinearLayout.LayoutParams.WRAP_CONTENT,
                            dp(34),
                        )
                        .apply {
                            gravity = Gravity.CENTER_VERTICAL
                            setMargins(dp(3), 0, dp(3), 0)
                        }
                bar.addView(v, lp)
            }
        }
        if (!composing && lastDeletedText.isNotEmpty()) {
            val undo = lastDeletedText
            addBarItem(
                label("↩ 撤销删除", highlight = true) {
                    currentInputConnection?.commitText(undo, 1)
                    invalidateSurroundingCache()
                    lastDeletedText = ""
                    candidates()
                }
            )
        }
        if (!composing && !passwordEditor && mode == "keys" && predictionCandidates.isNotEmpty()) {
            val maxChipW = resources.displayMetrics.widthPixels * 2 / 3
            predictionCandidates.forEachIndexed { idx, choice ->
                val chipText = if (idx == 0) "✨ $choice" else choice
                addBarItem(
                    label(chipText, highlight = idx == 0, subtle = idx > 0) {
                        acceptPrediction(choice)
                    }.apply {
                        setSingleLine(true)
                        maxWidth = maxChipW
                        ellipsize = android.text.TextUtils.TruncateAt.END
                        contentDescription = "预测补全：$choice"
                    }
                )
            }
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
                label(
                        display,
                        highlight = (i == 0 && composing) || isQuickPaste || isFunc,
                    ) {
                        selectCandidateDirect(item.getInt("index")) {
                            applyAction(it)
                        }
                    }
                    .apply {
                        val maxW =
                            if (isQuickPaste)
                                (sheet.width.takeIf { it > 0 }
                                    ?: resources.displayMetrics.widthPixels) * 3 / 4
                            else
                                ((sheet.width.takeIf { it > 0 }
                                        ?: resources.displayMetrics.widthPixels) / 3)
                                    .coerceAtLeast(dp(56))
                        maxWidth = maxW
                        setSingleLine(true)
                        ellipsize = android.text.TextUtils.TruncateAt.END
                        contentDescription =
                            (if (isQuickPaste) "快捷粘贴：" else "") + item.getString("text")
                        bindCandidateMenu(this, item)
                    }
            addBarItem(view)
        }
        if (hasPreviousPage)
            addBarItem(
                label("‹", subtle = true) {
                    processKeyDirect(0xff55) { applyAction(it) }
                }
            )
        if (hasNextPage)
            addBarItem(
                label("›", subtle = true) {
                    processKeyDirect(0xff56) { applyAction(it) }
                }
            )
        if (composing && (shown.length() >= 4 || hasNextPage || hasPreviousPage))
            addBarItem(
                label("⌄", highlight = true) {
                    mode = "candidates"
                    render()
                }.apply { contentDescription = "展开候选词" }
            )

        // When idle & empty, offer sleek quick-punctuation chips prioritizing recently used symbols
        if (
            !composing &&
                !inlineSuggestionsActive &&
                shown.length() == 0 &&
                predictionText.isEmpty() &&
                lastDeletedText.isEmpty()
        ) {
            val basePunc =
                when {
                    fieldKind == "email" && english -> listOf("@", ".", "_", "-", ".com", ".cn", "gmail.com")
                    fieldKind == "uri" && english -> listOf("https://", ".", "/", ":", "?", "=", "-", ".com")
                    english -> listOf(",", ".", "?", "!", "@", "/", "-", "_", ":", "#")
                    else -> listOf("，", "。", "？", "！", "、", "：", "……", "（）", "“”", "《》")
                }
            val quickPunc =
                if (fieldKind == "text") {
                    val recent = storedRecentSymbols().filter { sym ->
                        if (english) sym.all { it.code < 128 } else true
                    }
                    (recent + basePunc).distinct().take(12)
                } else {
                    basePunc
                }
            quickPunc.forEach { p ->
                addBarItem(
                    label(p, subtle = true) {
                        commitSmartText(p)
                        if (fieldKind == "text") recordSymbol(p)
                    }.apply {
                        setPadding(dp(12), dp(2), dp(12), dp(2))
                    },
                    marginHorizontalDp = 1,
                )
            }
        }
    }

    private fun commitSmartText(text: String) {
        val ic = currentInputConnection ?: return
        ic.commitText(text, 1)
        invalidateSurroundingCache()
        if (text in setOf("（）", "“”", "《》", "「」", "『』", "【】", "〈〉", "〔〕", "()", "\"\"", "[]", "{}")) {
            sendDownUpKeyEvents(KeyEvent.KEYCODE_DPAD_LEFT)
        }
    }

    private fun currentPreeditDisplay(): PreeditDisplay =
        buildPreeditDisplay(
            preeditText,
            shown.optJSONObject(0)?.optString("comment").orEmpty(),
            preeditCursorUtf8,
        )

    private fun displayPreedit(): String = currentPreeditDisplay().text

    private fun preeditEditingMidString(): Boolean =
        preeditText.isNotEmpty() &&
            preeditCursorUtf8 < preeditText.toByteArray(Charsets.UTF_8).size

    private fun canInsertDelimiterAtCursor(): Boolean {
        if (!composing || english || preeditText.isEmpty()) return false
        val bytes = preeditText.toByteArray(Charsets.UTF_8)
        val pos = preeditCursorUtf8.coerceIn(0, bytes.size)
        if (pos == 0) return false
        return bytes[pos - 1].toInt() != '\''.code
    }

    private fun preeditPillBackground(dark: Boolean): GradientDrawable? {
        if (!preeditEditingMidString()) return null
        return pillDrawable(
            if (dark) T.Dark.accentSoftArgb else T.Light.accentSoftArgb,
            T.Radius.candidateActivePillPx,
            if (dark) 0x6638BDF8L else 0x550284C7L,
        )
    }

    private fun styledPreeditText(): CharSequence {
        val display = currentPreeditDisplay()
        if (display.text.isEmpty()) return ""
        val dark = isDarkTheme
        val caretColor =
            (if (dark) T.Dark.accentPrimaryArgb else T.Light.accentPrimaryArgb).toInt()
        val caretPos = display.cursorIndex.coerceIn(0, display.text.length)
        return SpannableStringBuilder(display.text).apply {
            insert(caretPos, "|")
            setSpan(
                ForegroundColorSpan(caretColor),
                caretPos,
                caretPos + 1,
                Spanned.SPAN_EXCLUSIVE_EXCLUSIVE,
            )
            setSpan(
                StyleSpan(Typeface.BOLD),
                caretPos,
                caretPos + 1,
                Spanned.SPAN_EXCLUSIVE_EXCLUSIVE,
            )
        }
    }

    private fun bindEditablePreeditTap(view: TextView) {
        view.contentDescription = "点击定位拼音光标"
        view.setOnTouchListener { v, event ->
            if (event.actionMasked == MotionEvent.ACTION_UP && composing && preeditText.isNotEmpty()) {
                val tv = v as TextView
                val tapOffset = tv.getOffsetForPosition(event.x, event.y)
                val targetUtf8 =
                    visualTapToRawByteOffset(currentPreeditDisplay(), tapOffset, hasCaret = true)
                vibrator?.vibrate(
                    VibrationEffect.createOneShot(
                        T.Ergonomics.hapticLetterTickMs.toLong(),
                        VibrationEffect.DEFAULT_AMPLITUDE,
                    )
                )
                call("set-preedit-cursor", json("cursor_utf8" to targetUtf8)) {
                    applyAction(it)
                    if (mode == "candidates") showCandidateGrid()
                }
                true
            } else {
                event.actionMasked == MotionEvent.ACTION_DOWN
            }
        }
    }

    private fun bindCandidateMenu(view: View, item: JSONObject) {
        if (item.has("quick_paste_id") || item.has("function_action")) return
        val index = item.getInt("index")
        view.setOnLongClickListener { anchor ->
            PopupMenu(this, anchor).apply {
                menu.add("置顶此词").setOnMenuItemClickListener {
                    pinCandidateDirect(index) {
                        applyAction(it)
                        if (mode == "candidates") showCandidateGrid()
                    }
                    true
                }
                menu.add("移除此词").setOnMenuItemClickListener {
                    eraseCandidateDirect(index) {
                        applyAction(it)
                        if (mode == "candidates") showCandidateGrid()
                    }
                    true
                }
            }.show()
            true
        }
    }

    private fun candidateGridSpan(text: String): Int {
        var units = 0
        for (ch in text) {
            units += if (ch.code > 127) 2 else 1
        }
        return when {
            units <= 4 -> 1
            units <= 10 -> 2
            units <= 16 -> 3
            else -> 4
        }
    }

    private fun showCandidateGrid() {
        val dark = isDarkTheme
        body.removeAllViews()
        val scroll = ScrollView(this).apply { isVerticalScrollBarEnabled = false }
        val grid =
            LinearLayout(this).apply {
                orientation = LinearLayout.VERTICAL
                setPadding(dp(6), dp(6), dp(6), dp(6))
            }
        scroll.addView(grid)
        body.addView(scroll, LinearLayout.LayoutParams(-1, -1))
        val cols = 4
        var idx = 0
        while (idx < shown.length()) {
            val row =
                LinearLayout(this).apply {
                    weightSum = cols.toFloat()
                }
            var usedCols = 0
            while (idx < shown.length() && usedCols < cols) {
                val item = shown.getJSONObject(idx)
                val text = item.getString("text")
                val span = candidateGridSpan(text).coerceAtMost(cols)
                if (usedCols > 0 && usedCols + span > cols) break
                val pos = idx
                val isUser = item.optBoolean("is_user_phrase", false)
                val displayText = if (isUser) "$text ·" else text
                val cell =
                    label(displayText, highlight = pos == 0) {
                        selectCandidateDirect(item.getInt("index")) { r ->
                            applyAction(r)
                            if (mode == "candidates") {
                                showCandidateGrid()
                            }
                        }
                    }.apply {
                        bindCandidateMenu(this, item)
                        setSingleLine(true)
                        ellipsize = android.text.TextUtils.TruncateAt.END
                        contentDescription = if (isUser) "$text（自学习词）" else text
                        background =
                            pillDrawable(
                                if (pos == 0) {
                                    if (dark) T.Dark.accentSoftArgb else T.Light.accentSoftArgb
                                } else {
                                    if (dark) 0x12FFFFFFL else 0xE2FFFFFFL
                                },
                                10,
                                if (pos == 0) (if (dark) 0x450EA5E9L else 0x350284C7L)
                                else (if (dark) 0x16FFFFFFL else 0x120F172AL),
                            )
                    }
                row.addView(
                    cell,
                    LinearLayout.LayoutParams(0, dp(42), span.toFloat()).apply {
                        setMargins(dp(3), dp(3), dp(3), dp(3))
                    },
                )
                usedCols += span
                idx++
            }
            if (usedCols < cols) {
                row.addView(View(this), LinearLayout.LayoutParams(0, dp(42), (cols - usedCols).toFloat()))
            }
            grid.addView(row, LinearLayout.LayoutParams(-1, -2))
        }
        val pager =
            LinearLayout(this).apply {
                gravity = Gravity.CENTER
                setPadding(0, dp(6), 0, dp(4))
            }
        if (hasPreviousPage) {
            pager.addView(
                label("‹ 上一页", subtle = true) {
                    processKeyDirect(0xff55) {
                        applyAction(it)
                        if (mode == "candidates") showCandidateGrid()
                    }
                }
            )
        }
        if (hasNextPage) {
            pager.addView(
                label("下一页 ›", highlight = true) {
                    processKeyDirect(0xff56) {
                        applyAction(it)
                        if (mode == "candidates") showCandidateGrid()
                    }
                },
                LinearLayout.LayoutParams(-2, -2).apply { setMargins(dp(8), 0, 0, 0) },
            )
        }
        grid.addView(pager, LinearLayout.LayoutParams(-1, -2))
    }

    private fun storedRecentSymbols(): List<String> {
        val raw = getSharedPreferences("vitral", 0).getString("recent_symbols", "").orEmpty()
        return raw.split('\u001F').filter { it.isNotEmpty() }
    }

    private fun recentSymbols(): List<String> {
        val stored = storedRecentSymbols()
        val defaults =
            if (english) {
                listOf(
                    ",", ".", "?", "!", ":", ";", "-", "_",
                    "()", "\"\"", "'", "/", "@", "#", "$", "%",
                    "&", "*", "+", "=", "[]", "{}", "<", ">",
                )
            } else {
                listOf(
                    "，", "。", "？", "！", "、", "：", "；", "……",
                    "（）", "“”", "《》", "——", "【】", "「」", "~", "@",
                    "#", "￥", "%", "+", "-", "×", "÷", "=",
                )
            }
        return (stored + defaults).distinct().take(28)
    }

    private fun recordSymbol(sym: String) {
        if (sym.isEmpty()) return
        val next = (listOf(sym) + storedRecentSymbols().filter { it != sym }).take(28)
        getSharedPreferences("vitral", 0)
            .edit()
            .putString("recent_symbols", next.joinToString("\u001F"))
            .apply()
        idleStripSignature = null
    }

    private fun showSymbolPanel() {
        val dark = isDarkTheme
        body.removeAllViews()
        val list =
            when (symbolCategory) {
                1 ->
                    listOf(
                        "，", "。", "？", "！", "、", "：", "；",
                        "（）", "“”", "《》", "【】", "「」", "『』", "……",
                        "——", "‘", "’", "·", "～", "￥", "％",
                        "〈〉", "〔〕", "—", "｜", "＼", "／", "＃",
                    )
                2 ->
                    listOf(
                        ",", ".", "?", "!", ":", ";", "'",
                        "()", "\"\"", "[]", "{}", "<", ">", "-",
                        "_", "/", "\\", "|", "@", "#", "$",
                        "%", "^", "&", "*", "+", "=", "~",
                    )
                3 ->
                    listOf(
                        "+", "-", "×", "÷", "=", "≠", "≈",
                        "<", ">", "≤", "≥", "±", "√", "∞",
                        "%", "‰", "°", "℃", "π", "∑", "∫",
                        "α", "β", "γ", "Δ", "λ", "μ", "Ω",
                    )
                4 ->
                    listOf(
                        "①", "②", "③", "④", "⑤", "⑥", "⑦",
                        "⑧", "⑨", "⑩", "⑴", "⑵", "⑶", "⑷",
                        "⑸", "⒈", "⒉", "⒊", "⒋", "⒌", "Ⅰ",
                        "Ⅱ", "Ⅲ", "Ⅳ", "Ⅴ", "Ⅵ", "Ⅶ", "Ⅷ",
                    )
                5 ->
                    listOf(
                        "→", "←", "↑", "↓", "↔", "⇒", "⇔",
                        "✓", "✗", "★", "☆", "●", "○", "■",
                        "□", "▲", "△", "◆", "◇", "§", "※",
                        "©", "®", "™", "℃", "℉", "€", "£",
                    )
                else -> recentSymbols()
            }
        val scroll = ScrollView(this).apply { isVerticalScrollBarEnabled = false }
        val grid =
            LinearLayout(this).apply {
                orientation = LinearLayout.VERTICAL
                setPadding(dp(6), dp(6), dp(6), dp(6))
            }
        scroll.addView(grid)
        body.addView(scroll, LinearLayout.LayoutParams(-1, -1))
        val cols = 7
        var i = 0
        while (i < list.size) {
            val row = LinearLayout(this)
            for (c in 0 until cols) {
                if (i < list.size) {
                    val sym = list[i++]
                    val tv =
                        TextView(this).apply {
                            text = sym
                            textSize = if (sym.length >= 2) 15f else 17f
                            gravity = Gravity.CENTER
                            setTextColor(
                                (if (dark) T.Dark.textPrimaryArgb else T.Light.textPrimaryArgb).toInt()
                            )
                            background =
                                pillDrawable(
                                    if (dark) 0x12FFFFFFL else 0xE2FFFFFFL,
                                    10,
                                    if (dark) 0x16FFFFFFL else 0x120F172AL,
                                )
                            setOnClickListener {
                                commitSmartText(sym)
                                recordSymbol(sym)
                            }
                        }
                    row.addView(
                        tv,
                        LinearLayout.LayoutParams(0, dp(42), 1f).apply {
                            setMargins(dp(2), dp(2), dp(2), dp(2))
                        },
                    )
                } else {
                    row.addView(View(this), LinearLayout.LayoutParams(0, dp(42), 1f))
                }
            }
            grid.addView(row, LinearLayout.LayoutParams(-1, -2))
        }
    }

    private fun recentEmojis(): List<String> {
        val raw = getSharedPreferences("vitral", 0).getString("recent_emoji", "").orEmpty()
        val stored = raw.split(',').filter { it.isNotBlank() }
        val defaults =
            listOf(
                "😂", "👍", "❤️", "😊", "🙏", "🎉", "🔥", "✨",
                "😭", "🤔", "👌", "👏", "😅", "😍", "🤝", "💪",
                "🌟", "😎", "🥳", "🚀", "☕", "💡", "✅", "⚡",
            )
        return (stored + defaults).distinct().take(28)
    }

    private fun recordEmoji(emoji: String) {
        val next = (listOf(emoji) + recentEmojis().filter { it != emoji }).take(24)
        getSharedPreferences("vitral", 0)
            .edit()
            .putString("recent_emoji", next.joinToString(","))
            .apply()
    }

    private fun showEmojiPanel() {
        val dark = isDarkTheme
        body.removeAllViews()
        val list =
            when (emojiCategory) {
                1 ->
                    listOf(
                        "😀", "😃", "😄", "😁", "😆", "😅", "😂", "🤣",
                        "😊", "😇", "🙂", "😉", "😍", "🥰", "😘", "😋",
                        "😜", "🤪", "🤨", "🧐", "🤓", "😎", "🥳", "😏",
                        "😒", "😞", "😔", "😟", "😢", "😭", "😤", "😠",
                        "🤯", "😳", "🥵", "🥶", "😱", "😨", "🤔", "🤭",
                        "🤫", "🤥", "😶", "😴", "🤤", "🤢", "🤮", "🤧",
                    )
                2 ->
                    listOf(
                        "👍", "👎", "👏", "🙌", "👐", "🤲", "🤝", "🙏",
                        "✌️", "🤞", "🤟", "🤘", "👌", "🤌", "🤏", "👈",
                        "👉", "👆", "👇", "☝️", "✋", "🤚", "🖐️", "🖖",
                        "👋", "🤙", "💪", "✍️", "🤳", "💅", "👀", "🧠",
                    )
                3 ->
                    listOf(
                        "❤️", "🧡", "💛", "💚", "💙", "💜", "🖤", "🤍",
                        "💔", "❣️", "💕", "💞", "✨", "⭐", "🌟", "💫",
                        "🔥", "💥", "⚡", "🎉", "🎊", "🎈", "🎁", "🏆",
                        "✅", "❌", "⚠️", "❓", "❗", "💡", "📌", "🚀",
                    )
                else -> recentEmojis()
            }
        val scroll = ScrollView(this).apply { isVerticalScrollBarEnabled = false }
        val grid =
            LinearLayout(this).apply {
                orientation = LinearLayout.VERTICAL
                setPadding(dp(4), dp(4), dp(4), dp(4))
            }
        scroll.addView(grid)
        body.addView(scroll, LinearLayout.LayoutParams(-1, -1))
        val cols = 7
        var i = 0
        while (i < list.size) {
            val row = LinearLayout(this)
            for (c in 0 until cols) {
                if (i < list.size) {
                    val em = list[i++]
                    val tv =
                        TextView(this).apply {
                            text = em
                            textSize = 22f
                            gravity = Gravity.CENTER
                            background =
                                pillDrawable(
                                    if (dark) 0x08FFFFFFL else 0x55FFFFFFL,
                                    10,
                                )
                            setOnClickListener {
                                currentInputConnection?.commitText(em, 1)
                                invalidateSurroundingCache()
                                recordEmoji(em)
                            }
                        }
                    row.addView(
                        tv,
                        LinearLayout.LayoutParams(0, dp(42), 1f).apply {
                            setMargins(dp(2), dp(2), dp(2), dp(2))
                        },
                    )
                } else {
                    row.addView(View(this), LinearLayout.LayoutParams(0, dp(42), 1f))
                }
            }
            grid.addView(row, LinearLayout.LayoutParams(-1, -2))
        }
    }

    private fun sendEditKey(keyCode: Int) {
        val ic = currentInputConnection ?: return
        val now = SystemClock.uptimeMillis()
        val meta = if (editSelectMode) KeyEvent.META_SHIFT_ON else 0
        ic.sendKeyEvent(KeyEvent(now, now, KeyEvent.ACTION_DOWN, keyCode, 0, meta))
        ic.sendKeyEvent(KeyEvent(now, now, KeyEvent.ACTION_UP, keyCode, 0, meta))
    }

    private fun showTextEditPanel() {
        val dark = isDarkTheme
        body.removeAllViews()
        val root =
            LinearLayout(this).apply {
                orientation = LinearLayout.HORIZONTAL
                setPadding(dp(8), dp(8), dp(8), dp(8))
            }
        body.addView(root, LinearLayout.LayoutParams(-1, -1))

        fun editBtn(text: String, active: Boolean = false, accent: Boolean = false, click: () -> Unit): TextView =
            TextView(this).apply {
                this.text = text
                textSize = 14f
                typeface = Typeface.create("sans-serif-medium", if (active || accent) Typeface.BOLD else Typeface.NORMAL)
                gravity = Gravity.CENTER
                setTextColor(
                    (when {
                            accent -> 0xFFFFFFFFL
                            active && dark -> T.Dark.accentTextArgb
                            active -> T.Light.accentTextArgb
                            dark -> T.Dark.textPrimaryArgb
                            else -> T.Light.textPrimaryArgb
                        })
                        .toInt()
                )
                background =
                    pillDrawable(
                        when {
                            accent -> if (dark) T.Dark.accentPrimaryArgb else T.Light.accentPrimaryArgb
                            active -> if (dark) T.Dark.accentSoftArgb else T.Light.accentSoftArgb
                            dark -> 0x16FFFFFFL
                            else -> 0xEAFFFFFFL
                        },
                        12,
                        if (active || accent) (if (dark) 0x450EA5E9L else 0x350284C7L)
                        else (if (dark) 0x1EFFFFFFL else 0x160F172AL),
                    )
                setOnClickListener { click() }
            }

        // Left: 3x3 D-pad (Home / Up / End, Left / Select / Right, Del / Down / Paste)
        val pad = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        val padRows =
            listOf(
                listOf("行首" to { sendEditKey(KeyEvent.KEYCODE_MOVE_HOME) }, "↑" to { sendEditKey(KeyEvent.KEYCODE_DPAD_UP) }, "行尾" to { sendEditKey(KeyEvent.KEYCODE_MOVE_END) }),
                listOf("←" to { sendEditKey(KeyEvent.KEYCODE_DPAD_LEFT) }, (if (editSelectMode) "选择中" else "选择") to {
                    editSelectMode = !editSelectMode
                    candidates()
                    showTextEditPanel()
                }, "→" to { sendEditKey(KeyEvent.KEYCODE_DPAD_RIGHT) }),
                listOf("全选" to { currentInputConnection?.performContextMenuAction(android.R.id.selectAll) }, "↓" to { sendEditKey(KeyEvent.KEYCODE_DPAD_DOWN) }, "⌫" to {
                    currentInputConnection?.let { deleteWithSelectionOrSingle(it) }
                }),
            )
        padRows.forEach { rowItems ->
            val r = LinearLayout(this)
            rowItems.forEach { (title, act) ->
                val isSel = title.startsWith("选择") && editSelectMode
                r.addView(
                    editBtn(title, active = isSel) { act() },
                    LinearLayout.LayoutParams(0, -1, 1f).apply { setMargins(dp(3), dp(3), dp(3), dp(3)) },
                )
            }
            pad.addView(r, LinearLayout.LayoutParams(-1, 0, 1f))
        }
        root.addView(pad, LinearLayout.LayoutParams(0, -1, 3f))

        // Right column: Copy / Cut / Paste
        val actions = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        val actItems =
            listOf(
                "复制" to {
                    currentInputConnection?.performContextMenuAction(android.R.id.copy)
                    editSelectMode = false
                    message("已复制")
                },
                "剪切" to {
                    currentInputConnection?.performContextMenuAction(android.R.id.cut)
                    invalidateSurroundingCache()
                    editSelectMode = false
                    message("已剪切")
                },
                "粘贴" to {
                    currentInputConnection?.performContextMenuAction(android.R.id.paste)
                    invalidateSurroundingCache()
                    editSelectMode = false
                },
            )
        actItems.forEach { (title, act) ->
            actions.addView(
                editBtn(title, accent = title == "粘贴") { act() },
                LinearLayout.LayoutParams(-1, 0, 1f).apply { setMargins(dp(5), dp(3), dp(2), dp(3)) },
            )
        }
        root.addView(actions, LinearLayout.LayoutParams(0, -1, 1.15f))
    }

    private fun syncClipboardInBackground() {
        Core.call(this, "clipboard-poll", background = true)
    }

    private fun startEditShortcutCode(item: ClipboardItemSnapshot) {
        if (!canEditClipboardShortcut(item)) return
        clipboardScrollView?.scrollY?.let { clipboardScrollY = it }
        clipboardScrollView = null
        editingShortcutItemId = item.id
        editingShortcutContent = item.content
        editingShortcutCode = normalizeShortcutCode(item.shortcutCode.orEmpty())
        mode = "clipboard_shortcut"
        layoutPanel()
        render()
    }

    private fun returnToClipboardFromShortcutEditor() {
        clearShortcutEditorState()
        mode = "clipboard"
        layoutPanel()
        render()
    }

    private fun updateShortcutCodeDisplay() {
        val dark = isDarkTheme
        val display = shortcutCodeDisplay ?: return
        if (editingShortcutCode.isEmpty()) {
            display.text = "点下方字母输入简码（留空清除）"
            display.textSize = 12.5f
            display.typeface = Typeface.create("sans-serif", Typeface.NORMAL)
            display.setTextColor(
                (if (dark) T.Dark.textSecondaryArgb else T.Light.textSecondaryArgb).toInt()
            )
        } else {
            display.text = "${editingShortcutCode}│"
            display.textSize = 16f
            display.typeface = Typeface.create(Typeface.MONOSPACE, Typeface.BOLD)
            display.setTextColor(
                (if (dark) T.Dark.textPrimaryArgb else T.Light.textPrimaryArgb).toInt()
            )
        }
        shortcutClearBtn?.visibility = if (editingShortcutCode.isNotEmpty()) View.VISIBLE else View.GONE
    }

    private fun onShortcutEditorKey(label: String) {
        val duration =
            if (label.length == 1 && label[0].isLetter()) T.Ergonomics.hapticLetterTickMs
            else T.Ergonomics.hapticActionThudMs
        vibrator?.vibrate(
            VibrationEffect.createOneShot(duration.toLong(), VibrationEffect.DEFAULT_AMPLITUDE)
        )
        val next = applyShortcutEditorKey(editingShortcutCode, label)
        if (next != editingShortcutCode) {
            editingShortcutCode = next
            updateShortcutCodeDisplay()
        }
    }

    private fun saveEditingShortcutCode() {
        val itemId = editingShortcutItemId
        if (itemId.isEmpty()) {
            returnToClipboardFromShortcutEditor()
            return
        }
        val normalized = normalizeShortcutCode(editingShortcutCode)
        val seq = ++clipboardMutationSeq
        val current = clipboardItems.orEmpty()
        val optimistic =
            applyOptimisticPin(
                current,
                itemId,
                pinned = true,
                shortcutCode = normalized.ifEmpty { null },
                updateShortcut = true,
            )
        clipboardItems = optimistic
        val id = editor
        Core.call(
            this,
            "clipboard-pin",
            json("id" to itemId, "shortcut_code" to normalized),
        ) { result ->
            if (!active || editor != id) return@call
            result
                .onSuccess { v ->
                    syncClipboardInBackground()
                    if (v is JSONObject && seq == clipboardMutationSeq) {
                        val parsed = parseClipboardRing(v)
                        val changed = parsed != clipboardItems
                        clipboardItems = parsed
                        if (mode == "clipboard" && changed) showRing(parsed)
                    }
                }
                .onFailure { e ->
                    message(e.message.orEmpty())
                    if (seq == clipboardMutationSeq && mode == "clipboard") {
                        call("clipboard-state") {
                            if (mode == "clipboard" && seq == clipboardMutationSeq) {
                                val parsed = parseClipboardRing(it)
                                clipboardItems = parsed
                                showRing(parsed)
                            }
                        }
                    }
                }
        }
        clearShortcutEditorState()
        mode = "clipboard"
        layoutPanel()
        render()
    }

    private fun showShortcutCodeEditor() {
        val dark = isDarkTheme
        clipboardScrollView = null
        body.removeAllViews()

        val inputBar =
            LinearLayout(this).apply {
                orientation = LinearLayout.HORIZONTAL
                gravity = Gravity.CENTER_VERTICAL
                setPadding(dp(8), dp(5), dp(6), dp(5))
                background =
                    pillDrawable(
                        if (dark) 0x1AFFFFFFL else 0xF2FFFFFFL,
                        12,
                        if (dark) 0x450EA5E9L else 0x350284C7L,
                    )
            }
        inputBar.addView(
            label("简码", highlight = true) {}.apply {
                textSize = 11.5f
                setPadding(dp(8), dp(2), dp(8), dp(2))
                isClickable = false
                isFocusable = false
            },
            LinearLayout.LayoutParams(-2, -2).apply { setMargins(0, 0, dp(8), 0) },
        )
        val display =
            TextView(this).apply {
                setSingleLine(true)
                ellipsize = android.text.TextUtils.TruncateAt.START
                gravity = Gravity.CENTER_VERTICAL
                contentDescription = "收藏简码输入区"
            }
        shortcutCodeDisplay = display
        inputBar.addView(display, LinearLayout.LayoutParams(0, -2, 1f))

        val clearBtn =
            label("✕", subtle = true) { onShortcutEditorKey("清空") }.apply {
                contentDescription = "清空简码"
                setPadding(dp(8), dp(3), dp(8), dp(3))
            }
        shortcutClearBtn = clearBtn
        inputBar.addView(
            clearBtn,
            LinearLayout.LayoutParams(-2, -2).apply { setMargins(dp(4), 0, dp(4), 0) },
        )
        inputBar.addView(
            label("保存", solidAccent = true) { saveEditingShortcutCode() }.apply {
                textSize = 13f
                setPadding(dp(12), dp(5), dp(12), dp(5))
                contentDescription = "保存简码"
            }
        )
        body.addView(
            inputBar,
            LinearLayout.LayoutParams(-1, dp(42)).apply { setMargins(dp(3), dp(3), dp(3), dp(4)) },
        )
        updateShortcutCodeDisplay()

        fun addShortcutRow(
            keys: List<String>,
            sideSpacerWeight: Float = 0f,
            weightFn: (String) -> Float = { 1f },
        ) {
            val line = LinearLayout(this)
            body.addView(line, LinearLayout.LayoutParams(-1, 0, 1f))
            if (sideSpacerWeight > 0f) {
                line.addView(View(this), LinearLayout.LayoutParams(0, -1, sideSpacerWeight))
            }
            keys.forEach { s ->
                val role = if (s == "清空" || s == "⌫") KeyRole.FUNCTION else KeyRole.PRIMARY
                val key =
                    Key(
                        this,
                        s,
                        flypy = false,
                        upper = "",
                        action = ::onShortcutEditorKey,
                        voice = null,
                        role = role,
                        displayLabel = s,
                        shiftState = 0,
                        spaceHint = "",
                        darkOverride = dark,
                        sublabelMode = SublabelMode.CLEAN,
                    )
                line.addView(key, LinearLayout.LayoutParams(0, -1, weightFn(s)))
            }
            if (sideSpacerWeight > 0f) {
                line.addView(View(this), LinearLayout.LayoutParams(0, -1, sideSpacerWeight))
            }
        }

        addShortcutRow(rows[0].map { it.toString() })
        addShortcutRow(rows[1].map { it.toString() }, sideSpacerWeight = 0.45f)
        addShortcutRow(listOf("清空") + rows[2].map { it.toString() } + listOf("⌫")) { s ->
            if (s == "清空" || s == "⌫") 1.45f else 1f
        }
    }

    private fun toggleClipboardPin(item: ClipboardItemSnapshot) {
        val nextPinned = !item.pinned
        val seq = ++clipboardMutationSeq
        val current = clipboardItems.orEmpty()
        val optimistic = applyOptimisticPin(current, item.id, pinned = nextPinned)
        clipboardItems = optimistic
        showRing(optimistic)

        val cmd = if (nextPinned) "clipboard-pin" else "clipboard-unpin"
        val id = editor
        Core.call(this, cmd, json("id" to item.id)) { result ->
            if (!active || editor != id) return@call
            result
                .onSuccess { v ->
                    syncClipboardInBackground()
                    if (v is JSONObject && seq == clipboardMutationSeq) {
                        val parsed = parseClipboardRing(v)
                        clipboardItems = parsed
                        if (mode == "clipboard" && parsed != optimistic) {
                            showRing(parsed)
                        }
                    }
                }
                .onFailure { e ->
                    message(e.message.orEmpty())
                    if (seq == clipboardMutationSeq && mode == "clipboard") {
                        call("clipboard-state") {
                            if (mode == "clipboard" && seq == clipboardMutationSeq) {
                                val parsed = parseClipboardRing(it)
                                clipboardItems = parsed
                                showRing(parsed)
                            }
                        }
                    }
                }
        }
    }

    private fun showRing(items: List<ClipboardItemSnapshot>) {
        val dark = isDarkTheme
        clipboardScrollView?.scrollY?.let { clipboardScrollY = it }
        val scroll = ScrollView(this)
        val list =
            LinearLayout(this).apply {
                orientation = LinearLayout.VERTICAL
                setPadding(dp(6), dp(4), dp(6), dp(6))
            }
        scroll.addView(list)
        body.removeAllViews()
        body.addView(scroll, -1, -1)
        clipboardScrollView = scroll

        if (items.isEmpty()) {
            clipboardScrollY = 0
            list.addView(
                TextView(this).apply {
                    text = "暂无剪贴板记录\n复制文本后将自动同步并显示在此处"
                    textSize = 13f
                    gravity = Gravity.CENTER
                    setPadding(dp(16), dp(28), dp(16), dp(28))
                    setTextColor(
                        (if (dark) T.Dark.textSecondaryArgb else T.Light.textSecondaryArgb).toInt()
                    )
                }
            )
            return
        }

        val headerRow =
            LinearLayout(this).apply {
                gravity = Gravity.CENTER_VERTICAL
                setPadding(dp(4), dp(2), dp(2), dp(6))
            }
        headerRow.addView(
            TextView(this).apply {
                text = "共 ${items.size} 条 · 点击粘贴 · 长按收藏设简码"
                textSize = 11.5f
                setTextColor(
                    (if (dark) T.Dark.textSecondaryArgb else T.Light.textSecondaryArgb).toInt()
                )
            },
            LinearLayout.LayoutParams(0, -2, 1f),
        )
        headerRow.addView(
            label("清空未置顶", subtle = true) {
                val seq = ++clipboardMutationSeq
                val optimistic = applyOptimisticClearUnpinned(clipboardItems.orEmpty())
                clipboardItems = optimistic
                showRing(optimistic)
                val id = editor
                Core.call(this, "clipboard-clear-unpinned") { result ->
                    if (!active || editor != id) return@call
                    result
                        .onSuccess { v ->
                            syncClipboardInBackground()
                            if (v is JSONObject && seq == clipboardMutationSeq) {
                                val parsed = parseClipboardRing(v)
                                clipboardItems = parsed
                                if (mode == "clipboard" && parsed != optimistic) {
                                    showRing(parsed)
                                }
                            }
                        }
                        .onFailure { e ->
                            message(e.message.orEmpty())
                            if (seq == clipboardMutationSeq && mode == "clipboard") {
                                call("clipboard-state") {
                                    if (mode == "clipboard" && seq == clipboardMutationSeq) {
                                        val parsed = parseClipboardRing(it)
                                        clipboardItems = parsed
                                        showRing(parsed)
                                    }
                                }
                            }
                        }
                }
            }
        )
        list.addView(headerRow)

        for (pinnedGroup in listOf(true, false)) {
            var headerAdded = false
            for (item in items) {
                val isPinned = item.pinned
                if (isPinned != pinnedGroup) continue
                if (!headerAdded) {
                    headerAdded = true
                    list.addView(
                        TextView(this).apply {
                            text = if (pinnedGroup) "置顶收藏" else "最近复制"
                            textSize = 11.5f
                            typeface = Typeface.create("sans-serif-medium", Typeface.BOLD)
                            setPadding(dp(4), dp(6), dp(4), dp(4))
                            setTextColor(
                                (if (pinnedGroup) {
                                        if (dark) T.Dark.accentTextArgb else T.Light.accentTextArgb
                                    } else {
                                        if (dark) T.Dark.textSecondaryArgb else T.Light.textSecondaryArgb
                                    })
                                    .toInt()
                            )
                        }
                    )
                }

                val card =
                    LinearLayout(this).apply {
                        orientation = LinearLayout.HORIZONTAL
                        gravity = Gravity.CENTER_VERTICAL
                        setPadding(dp(11), dp(8), dp(6), dp(8))
                        background =
                            pillDrawable(
                                if (isPinned) {
                                    if (dark) 0x1AFFFFFFL else 0xF0FFFFFFL
                                } else {
                                    if (dark) 0x0EFFFFFFL else 0xC8FFFFFFL
                                },
                                12,
                                if (isPinned)
                                    (if (dark) 0x450EA5E9L else 0x350284C7L)
                                else (if (dark) 0x14FFFFFFL else 0x100F172AL),
                            )
                    }
                val content = item.content
                val codeText = item.shortcutCode
                if (codeText != null) {
                    val codeBadge =
                        label(codeText, highlight = true, mono = true) {
                            currentInputConnection?.commitText(content, 1)
                            invalidateSurroundingCache()
                            switch("keys")
                        }
                    if (canEditClipboardShortcut(item)) {
                        codeBadge.setOnLongClickListener {
                            startEditShortcutCode(item)
                            true
                        }
                    }
                    card.addView(
                        codeBadge,
                        LinearLayout.LayoutParams(-2, -2).apply { setMargins(0, 0, dp(8), 0) },
                    )
                }
                val textView =
                    TextView(this).apply {
                        text = content.replace('\n', ' ')
                        textSize = 13.5f
                        maxLines = 2
                        ellipsize = android.text.TextUtils.TruncateAt.END
                        setTextColor(
                            (if (dark) T.Dark.textPrimaryArgb else T.Light.textPrimaryArgb).toInt()
                        )
                        setOnClickListener {
                            currentInputConnection?.commitText(content, 1)
                            invalidateSurroundingCache()
                            switch("keys")
                        }
                        if (canEditClipboardShortcut(item)) {
                            setOnLongClickListener {
                                startEditShortcutCode(item)
                                true
                            }
                        }
                    }
                card.addView(textView, LinearLayout.LayoutParams(0, -2, 1f))

                val pinBtn =
                    ImageButton(this).apply {
                        setImageResource(R.drawable.pin)
                        scaleType = ImageView.ScaleType.CENTER_INSIDE
                        setColorFilter(
                            (when {
                                    isPinned && dark -> T.Dark.accentTextArgb
                                    isPinned -> T.Light.accentPrimaryArgb
                                    dark -> T.Dark.textSecondaryArgb
                                    else -> T.Light.textSecondaryArgb
                                })
                                .toInt()
                        )
                        background =
                            if (isPinned)
                                pillDrawable(
                                    if (dark) T.Dark.accentSoftArgb else T.Light.accentSoftArgb,
                                    9,
                                )
                            else null
                        contentDescription = if (isPinned) "取消置顶" else "置顶"
                        setPadding(dp(6), dp(5), dp(6), dp(5))
                        setOnClickListener {
                            performHapticFeedback(HapticFeedbackConstants.CONTEXT_CLICK)
                            toggleClipboardPin(item)
                        }
                    }
                card.addView(pinBtn, LinearLayout.LayoutParams(dp(34), dp(30)))

                val lp =
                    LinearLayout.LayoutParams(-1, -2).apply { setMargins(0, dp(3), 0, dp(3)) }
                list.addView(card, lp)
            }
        }
        val targetScrollY = clipboardScrollY
        if (targetScrollY > 0) {
            scroll.post { scroll.scrollTo(0, targetScrollY) }
        }
    }

    private fun showInk(r: JSONObject) {
        val dark = isDarkTheme
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
                val badge = if (code.isNotEmpty()) "$text  $py · $code" else "$text  $py"
                addBarItem(
                    label(badge, highlight = i > 0, solidAccent = i == 0) {
                        currentInputConnection?.commitText(text, 1)
                        invalidateSurroundingCache()
                        switch("lookup")
                    }
                )
            }
        } else {
            val inkComposing = r.optString("composing_text")
            currentInputConnection?.setComposingText(inkComposing, 1)
            hwComposing = inkComposing.isNotEmpty()
            val slots = r.optJSONArray("slots") ?: return
            val glowColor =
                (if (dark) T.Dark.retroactiveFixGlowArgb else T.Light.retroactiveFixGlowArgb).toInt()
            for (i in 0 until slots.length()) {
                val s = slots.getJSONObject(i)
                val choice =
                    label(s.getString("char"), highlight = true) {
                        bar.removeAllViews()
                        addBarItem(label("←", subtle = true) { lastInkResult?.let { showInk(it) } })
                        val options = s.getJSONArray("top8")
                        for (j in 0 until options.length()) {
                            addBarItem(
                                label(
                                    options.getJSONObject(j).getString("char"),
                                    solidAccent = j == 0,
                                ) {
                                    hwOverrideSlot(i, j)
                                }
                            )
                        }
                    }
                if (s.optBoolean("changed_retroactively")) {
                    choice.background =
                        pillDrawable(
                            glowColor.toLong() and 0xFFFFFFFFL,
                            T.Radius.candidateActivePillPx,
                        )
                    choice.postDelayed(
                        {
                            choice.background =
                                pillDrawable(
                                    if (dark) T.Dark.accentSoftArgb else T.Light.accentSoftArgb,
                                    T.Radius.candidateActivePillPx,
                                )
                        },
                        T.Handwriting.retroactiveHighlightDurationMs.toLong(),
                    )
                }
                addBarItem(choice)
            }
        }
    }

    private fun hwOverrideSlot(slot: Int, idx: Int) {
        val gen = canvasGeneration
        call("hw-override-slot", json("slot_idx" to slot, "candidate_idx" to idx)) {
            if (gen == canvasGeneration) showInk(it)
        }
    }

    private fun refreshKeyboardCase(force: Boolean = false) {
        if (!::body.isInitialized) return
        if (
            !force &&
                lastKeyboardCaseComposing == composing &&
                lastKeyboardCaseEnglish == english &&
                lastKeyboardCaseShift == shiftState
        ) {
            return
        }
        lastKeyboardCaseComposing = composing
        lastKeyboardCaseEnglish = english
        lastKeyboardCaseShift = shiftState
        fun refresh(view: View) {
            when (view) {
                is Key -> {
                    view.onCompositionChanged(composing)
                    view.refreshCase(english, shiftState)
                }
                is AeviouKeyboard -> {
                    view.onCompositionChanged(composing)
                    view.refreshCase(shiftState)
                }
                is android.view.ViewGroup -> for (i in 0 until view.childCount) refresh(view.getChildAt(i))
            }
        }
        refresh(body)
    }

    private fun keyboard() {
        val dark = isDarkTheme

        fun roleFor(s: String): KeyRole =
            when (s) {
                "↵" -> KeyRole.ACCENT
                "123",
                "ABC",
                "符号",
                "⇧",
                "⌫",
                "空格",
                "，",
                "。",
                ",",
                "." -> KeyRole.FUNCTION
                else -> KeyRole.PRIMARY
            }

        fun addKeyRow(
            keys: List<String>,
            altRow: Int = -1,
            sideSpacerWeight: Float = 0f,
            upperOverride: ((String) -> String)? = null,
            weightFn: (String) -> Float = { 1f },
        ) {
            val line = LinearLayout(this)
            body.addView(line, LinearLayout.LayoutParams(-1, 0, 1f))
            if (sideSpacerWeight > 0f) {
                line.addView(View(this), LinearLayout.LayoutParams(0, -1, sideSpacerWeight))
            }
            var letterCol = 0
            keys.forEach { s ->
                val isLetter = s.length == 1 && s[0] in 'a'..'z'
                val upper =
                    upperOverride?.invoke(s)
                        ?: if (altRow >= 0 && isLetter) alternate(altRow, letterCol++) else ""
                // Uppercase letters in Chinese mode (clean zero-descender symmetry matching prototype);
                // dynamic case in English mode following Shift / CapsLock.
                val display =
                    when {
                        s == "↵" -> enterLabel
                        isLetter && (!english || shiftState > 0) -> s.uppercase()
                        else -> s
                    }
                val spaceHint =
                    if (s == "空格") {
                        when {
                            english -> "English"
                            flypy -> "Vitral · 双拼"
                            else -> "Vitral · 全拼"
                        }
                    } else ""
                val key =
                    Key(
                        this,
                        s,
                        flypy && !english,
                        upper,
                        ::key,
                        if (s == "空格") ::voice else null,
                        roleFor(s),
                        displayLabel = display,
                        shiftState = if (s == "⇧") shiftState else 0,
                        spaceHint = spaceHint,
                        darkOverride = dark,
                        sublabelMode = sublabelMode,
                        isComposing = { composing },
                        canScrubSpace = { true },
                    )
                line.addView(key, LinearLayout.LayoutParams(0, -1, weightFn(s)))
            }
            if (sideSpacerWeight > 0f) {
                line.addView(View(this), LinearLayout.LayoutParams(0, -1, sideSpacerWeight))
            }
        }

        if (numbers) {
            addKeyRow(listOf("+", "1", "2", "3", "⌫")) { if (it == "+" || it == "⌫") 1.05f else 1.25f }
            addKeyRow(listOf("-", "4", "5", "6", "符号")) { if (it == "-" || it == "符号") 1.05f else 1.25f }
            addKeyRow(listOf("×", "7", "8", "9", ".")) { if (it == "×" || it == ".") 1.05f else 1.25f }
            addKeyRow(listOf("ABC", "÷", "0", "空格", "↵")) {
                when (it) {
                    "ABC", "↵", "0", "空格" -> 1.25f
                    else -> 1.05f
                }
            }
            return
        }

        val count = if (flypy) T.Ergonomics.flypyRows else T.Ergonomics.quanpinRows
        if (!flypy && aeviouExperimental) {
            val view =
                AeviouKeyboard(
                    this,
                    env = effectiveAeviouEnv(),
                    shiftState = shiftState,
                    leftHanded = aeviouLeftHanded,
                    darkOverride = dark,
                    onSyllable = { syl ->
                        commitAeviouSyllable(syl)
                    },
                    onActionKey = { act -> key(act) },
                    isComposing = { composing },
                )
            aeviouView = view
            refreshAeviouContextHints()
            body.addView(view, LinearLayout.LayoutParams(-1, -1))
            return
        }
        aeviouView = null
        if (count == 5) {
            addKeyRow("1234567890".map { it.toString() })
        }
        addKeyRow(rows[0].map { it.toString() }, altRow = 0)
        addKeyRow(rows[1].map { it.toString() }, altRow = 1, sideSpacerWeight = 0.45f)
        addKeyRow(listOf("⇧") + rows[2].map { it.toString() } + listOf("⌫"), altRow = 2) { s ->
            when (s) {
                "⇧", "⌫" -> 1.45f
                else -> 1f
            }
        }
        val leftPunct =
            when {
                fieldKind == "email" && english -> "@"
                fieldKind == "uri" && english -> "/"
                english -> ","
                else -> "，"
            }
        val rightPunct = if (english) "." else "。"
        addKeyRow(
            listOf("123", leftPunct, "空格", rightPunct, "↵"),
            upperOverride = { s ->
                when (s) {
                    "123" -> "符号"
                    "，" -> "？"
                    "。" -> "！"
                    "," -> "?"
                    "." -> if (fieldKind == "text") "!" else "-"
                    "@" -> "_"
                    "/" -> ":"
                    else -> ""
                }
            },
        ) { s ->
            when (s) {
                "空格" -> 3.6f
                "↵" -> 1.55f
                "123" -> 1.4f
                else -> 0.95f
            }
        }
    }

    private fun commitAeviouSyllable(syl: String) {
        if (syl.isEmpty()) return
        if (syl.length == 1) {
            key(if (syl[0] == ' ') "空格" else syl)
            return
        }
        cancelPrediction()
        inlineRequest++
        val ic = currentInputConnection ?: return
        vibrator?.vibrate(
            VibrationEffect.createOneShot(
                T.Ergonomics.hapticLetterTickMs.toLong(),
                VibrationEffect.DEFAULT_AMPLITUDE,
            )
        )
        val effective =
            if (english && shiftState > 0) {
                if (shiftState == 1) {
                    shiftState = 0
                    refreshKeyboardCase()
                    syl.replaceFirstChar { if (it in 'a'..'z') it.uppercaseChar() else it }
                } else {
                    syl.uppercase()
                }
            } else {
                syl
            }
        if (lastDeletedText.isNotEmpty()) lastDeletedText = ""
        lastSpaceTapMs =
            if (english && fieldKind == "text" && !passwordEditor && effective.endsWith(" "))
                SystemClock.uptimeMillis()
            else 0L
        if (fieldForceEnglish && !composing) {
            ic.commitText(effective, 1)
            invalidateSurroundingCache()
            updateAutoCap(redraw = true)
            return
        }
        runNativeAction(
            operation = { h ->
                val committed = StringBuilder()
                var cursorDelta = 0
                var last: JSONObject? = null
                var i = 0
                while (i < effective.length) {
                    val cp = effective.codePointAt(i)
                    i += Character.charCount(cp)
                    val act = unpack(VitralNative.processKey(h, cp, 0, 0)) as JSONObject
                    val c =
                        if (act.has("commit_text") && !act.isNull("commit_text"))
                            act.optString("commit_text")
                        else ""
                    if (c.isNotEmpty()) committed.append(c)
                    cursorDelta += act.optInt("cursor_delta", 0)
                    last = act
                }
                val merged = last ?: JSONObject()
                if (committed.isNotEmpty()) merged.put("commit_text", committed.toString())
                if (cursorDelta != 0) merged.put("cursor_delta", cursorDelta)
                merged
            }
        ) { r ->
            applyAction(r)
        }
    }

    private fun key(rawLabel: String) {
        val isRepeatDelete = rawLabel == "⌫-repeat"
        if (backspaceFlight.shouldDrop(isRepeatDelete)) return
        val label = if (isRepeatDelete) "⌫" else rawLabel
        cancelPrediction()
        inlineRequest++
        val ic = currentInputConnection ?: return
        val duration =
            if (label.length == 1 && label[0].isLetter()) T.Ergonomics.hapticLetterTickMs
            else T.Ergonomics.hapticActionThudMs
        vibrator?.vibrate(
            VibrationEffect.createOneShot(duration.toLong(), VibrationEffect.DEFAULT_AMPLITUDE)
        )
        when (label) {
            "123" -> {
                numbers = true
                layoutPanel()
                render()
                return
            }
            "符号" -> {
                numbers = false
                symbolCategory = 0
                switch("symbols")
                return
            }
            "ABC" -> {
                numbers = false
                layoutPanel()
                render()
                return
            }
            "☺" -> {
                switch("emoji")
                return
            }
            "（）", "“”", "《》", "「」", "『』", "【】", "……", "——" -> {
                if (composing) {
                    processKeyDirect(
                        32,
                        onSameEditorPanelSwitched = { commitSmartText(label) },
                    ) { r ->
                        applyAction(r)
                        commitSmartText(label)
                    }
                } else {
                    commitSmartText(label)
                }
                return
            }
            "⇧" -> {
                if (!english) {
                    processKeyDirect(0, MOD_LANG_TOGGLE) {
                        applyAction(it)
                    }
                } else {
                    val now = SystemClock.uptimeMillis()
                    shiftState =
                        when {
                            shiftState == 1 && now - lastShiftTapMs <= 340L -> 2
                            shiftState > 0 -> 0
                            else -> 1
                        }
                    lastShiftTapMs = now
                    refreshKeyboardCase()
                }
                return
            }
            "cancel-voice" -> {
                cancelVoice()
                return
            }
            "cursor-left",
            "cursor-right" -> {
                if (composing) {
                    processKeyDirect(if (label == "cursor-left") 0xff51 else 0xff53) {
                        applyAction(it)
                    }
                    return
                }
                sendDownUpKeyEvents(
                    if (label == "cursor-left") KeyEvent.KEYCODE_DPAD_LEFT
                    else KeyEvent.KEYCODE_DPAD_RIGHT
                )
                invalidateSurroundingCache()
                return
            }
            "delete-word" -> {
                deleteWordSafely(ic)
                return
            }
            "clear-all" -> {
                clearAllInputText(ic)
                return
            }
        }
        val effectiveLabel =
            if (english && shiftState > 0 && label.length == 1 && label[0] in 'a'..'z') {
                val out = label.uppercase()
                if (shiftState == 1) {
                    shiftState = 0
                    refreshKeyboardCase()
                }
                out
            } else {
                label
            }
        if (lastDeletedText.isNotEmpty() && effectiveLabel != "⌫") {
            lastDeletedText = ""
        }
        // English double-space -> ". " (standard mobile UX)
        if (
            effectiveLabel == "空格" &&
                english &&
                !composing &&
                fieldKind == "text" &&
                !passwordEditor
        ) {
            val now = SystemClock.uptimeMillis()
            if (now - lastSpaceTapMs <= 340L) {
                val before = ic.getTextBeforeCursor(3, 0)?.toString().orEmpty()
                if (
                    before.length >= 2 &&
                        before.last() == ' ' &&
                        before[before.length - 2].isLetterOrDigit()
                ) {
                    lastSpaceTapMs = 0L
                    ic.deleteSurroundingText(1, 0)
                    ic.commitText(". ", 1)
                    invalidateSurroundingCache()
                    updateAutoCap(redraw = true)
                    return
                }
            }
            lastSpaceTapMs = now
        } else {
            lastSpaceTapMs = 0L
        }
        // Password / email / URL fields bypass Chinese engine completely for zero leakage
        if (fieldForceEnglish && !composing) {
            when (effectiveLabel) {
                "⌫" -> deleteWithSelectionOrSingle(ic)
                "↵" ->
                    if (!sendDefaultEditorAction(true)) sendDownUpKeyEvents(KeyEvent.KEYCODE_ENTER)
                    else Unit
                "空格" -> {
                    ic.commitText(" ", 1)
                    invalidateSurroundingCache()
                }
                else -> {
                    ic.commitText(effectiveLabel, 1)
                    invalidateSurroundingCache()
                    updateAutoCap(redraw = true)
                }
            }
            return
        }
        val code =
            when (effectiveLabel) {
                "空格" -> "space"
                "⌫" -> "BackSpace"
                "↵" -> "Return"
                else -> effectiveLabel
            }
        val dismissPaste =
            effectiveLabel == "⌫" &&
                !composing &&
                shown.length() > 0 &&
                shown.getJSONObject(0).has("quick_paste_id")
        val startedComposing = composing
        val backspace = effectiveLabel == "⌫"
        val reqEditor = editor
        val generation = inputGeneration
        if (backspace) backspaceFlight.onDispatched()
        val onComplete = {
            if (backspace && editor == reqEditor && generation == inputGeneration) {
                backspaceFlight.onCompleted()
            }
        }
        val onResult: (JSONObject) -> Unit = { r ->
            when {
                dismissPaste -> {
                    applyAction(r)
                    call("quick-paste-state") { updatePaste(it) }
                }
                shouldDeleteHostText(composing, startedComposing, isRepeatDelete)
                    && effectiveLabel == "⌫" -> {
                    deleteWithSelectionOrSingle(ic)
                    updateAutoCap(redraw = true)
                }
                !composing && effectiveLabel == "↵" -> {
                    if (!sendDefaultEditorAction(true)) sendDownUpKeyEvents(KeyEvent.KEYCODE_ENTER)
                    updateAutoCap(redraw = true)
                }
                else -> applyAction(r)
            }
        }
        val nativeCode = nativeKeycode(code)
        if (nativeCode >= 0) {
            processKeyDirect(nativeCode, onComplete = onComplete, done = onResult)
        } else {
            call("process-key", json("keycode" to code), onComplete = onComplete, done = onResult)
        }
    }

    private fun recordDeletedForUndo(text: String) {
        if (!passwordEditor && text.isNotEmpty()) {
            lastDeletedText = text
            lastDeletedAtMs = SystemClock.uptimeMillis()
        }
    }

    private fun deleteWithSelectionOrSingle(ic: android.view.inputmethod.InputConnection) {
        val selected = ic.getSelectedText(0)?.toString().orEmpty()
        if (selected.isNotEmpty()) {
            if (ic.commitText("", 1)) {
                invalidateSurroundingCache()
                recordDeletedForUndo(selected)
                val extra = selected.codePointCount(0, selected.length) - 1
                if (!passwordEditor && extra > 0) {
                    Core.call(this, "notify-deleted-chars", json("count" to extra))
                }
                if (lastDeletedText.isNotEmpty()) candidates()
            }
        } else {
            ic.deleteSurroundingTextInCodePoints(1, 0)
            invalidateSurroundingCache()
        }
    }

    private fun deleteWordSafely(ic: android.view.inputmethod.InputConnection) {
        if (composing) {
            processKeyDirect(27) { applyAction(it) }
            return
        }
        val selected = ic.getSelectedText(0)?.toString().orEmpty()
        if (selected.isNotEmpty()) {
            if (ic.commitText("", 1)) {
                invalidateSurroundingCache()
                recordDeletedForUndo(selected)
                if (!passwordEditor) {
                    Core.call(
                        this,
                        "notify-deleted-chars",
                        json("count" to selected.codePointCount(0, selected.length)),
                    )
                }
                if (lastDeletedText.isNotEmpty()) candidates()
            }
            return
        }
        if (passwordEditor) {
            ic.deleteSurroundingTextInCodePoints(1, 0)
            invalidateSurroundingCache()
            return
        }
        val before = ic.getTextBeforeCursor(48, 0)?.toString().orEmpty()
        val (deleted, matchedLastCommit) = trailingWordDeleteSlice(before, lastCommittedText)
        if (deleted.isEmpty()) return
        if (matchedLastCommit) lastCommittedText = ""
        if (ic.deleteSurroundingText(deleted.length, 0)) {
            invalidateSurroundingCache()
            Core.call(
                this,
                "notify-deleted-chars",
                json("count" to deleted.codePointCount(0, deleted.length)),
            )
            recordDeletedForUndo(deleted)
            candidates()
        }
    }

    private fun clearAllInputText(ic: android.view.inputmethod.InputConnection) {
        ic.finishComposingText()
        val captured =
            if (!passwordEditor) {
                val full =
                    ic.getExtractedText(android.view.inputmethod.ExtractedTextRequest(), 0)
                        ?.text
                        ?.toString()
                if (!full.isNullOrEmpty()) {
                    full
                } else {
                    val before = ic.getTextBeforeCursor(100_000, 0)?.toString().orEmpty()
                    val selected = ic.getSelectedText(0)?.toString().orEmpty()
                    val after = ic.getTextAfterCursor(100_000, 0)?.toString().orEmpty()
                    before + selected + after
                }
            } else {
                ""
            }

        if (ic.performContextMenuAction(android.R.id.selectAll)) {
            ic.commitText("", 1)
        }
        val remBefore = ic.getTextBeforeCursor(100_000, 0)?.length ?: 0
        val remAfter = ic.getTextAfterCursor(100_000, 0)?.length ?: 0
        if (remBefore > 0 || remAfter > 0) {
            ic.beginBatchEdit()
            ic.setSelection(0, remBefore + remAfter)
            ic.commitText("", 1)
            val stillBefore = ic.getTextBeforeCursor(100_000, 0)?.length ?: 0
            val stillAfter = ic.getTextAfterCursor(100_000, 0)?.length ?: 0
            if (stillBefore > 0 || stillAfter > 0) {
                ic.deleteSurroundingText(stillBefore, stillAfter)
            }
            ic.endBatchEdit()
        }

        lastCommittedText = ""
        predictionText = ""
        predictionCandidates = emptyList()
        predictionContextAfter = ""
        invalidateSurroundingCache()
        if (composing) {
            processKeyDirect(27) {
                applyAction(it)
                recordDeletedForUndo(captured)
                candidates()
            }
        } else {
            recordDeletedForUndo(captured)
            candidates()
        }
        updateAutoCap(redraw = true)
    }

    private fun detectAeviouEnvFromContext(): AeviouEnv {
        if (fieldForceEnglish) return AeviouEnv.PURE_EN
        val ic = currentInputConnection
        val before = readBeforeCursor(ic, 48)
        val after = readAfterCursor(ic)
        val sample = "$before $after"
        var cjkCount = 0
        var enWordCount = 0
        var enLetterCount = 0
        var runLen = 0
        for (ch in sample) {
            when {
                ch in '\u4E00'..'\u9FFF' -> {
                    cjkCount++
                    if (runLen >= 2) enWordCount++
                    runLen = 0
                }
                ch in 'a'..'z' || ch in 'A'..'Z' -> {
                    enLetterCount++
                    runLen++
                }
                else -> {
                    if (runLen >= 2) enWordCount++
                    runLen = 0
                }
            }
        }
        if (runLen >= 2) enWordCount++
        return when {
            cjkCount == 0 && (enWordCount >= 2 || enLetterCount >= 8) -> AeviouEnv.PURE_EN
            cjkCount >= 1 && enWordCount >= 1 -> AeviouEnv.MIXED
            cjkCount >= 3 && enWordCount == 0 -> AeviouEnv.PURE_ZH
            english -> AeviouEnv.PURE_EN
            else -> AeviouEnv.MIXED
        }
    }

    private fun effectiveAeviouEnv(): AeviouEnv {
        if (fieldForceEnglish || english) return AeviouEnv.PURE_EN
        return if (detectedAeviouEnv == AeviouEnv.PURE_ZH) AeviouEnv.PURE_ZH else AeviouEnv.MIXED
    }

    private fun syncAutoAeviouEnv(triggerLangSync: Boolean, forceRender: Boolean = true): Boolean {
        if (flypy || !aeviouExperimental || composing) {
            return false
        }
        val next = detectAeviouEnvFromContext()
        val prevEffective = effectiveAeviouEnv()
        detectedAeviouEnv = next
        if (!manualLangOverride) {
            val wantEnglish = next == AeviouEnv.PURE_EN
            if (triggerLangSync && wantEnglish != english && !fieldForceEnglish) {
                processKeyDirect(0, MOD_LANG_TOGGLE) {
                    applyAction(it)
                }
                return true
            }
        }
        val nextEffective = effectiveAeviouEnv()
        if (nextEffective != prevEffective && forceRender && mode == "keys") {
            render()
            return true
        }
        return false
    }

    private fun buildEnglishContextHints(
        before: String,
        pred: String,
    ): LinkedHashMap<String, Triple<String, String, Int>> {
        val hints = LinkedHashMap<String, Triple<String, String, Int>>()
        val predWord = pred.trim().takeWhile { it in 'a'..'z' || it in 'A'..'Z' }
        if (predWord.length >= 2) {
            val init = predWord.take(1).lowercase()
            hints[init] = Triple(predWord.lowercase(), predWord, 0)
        }
        val lastWord =
            before
                .trimEnd()
                .takeLastWhile { it in 'a'..'z' || it in 'A'..'Z' }
                .lowercase()
        if (lastWord.isNotEmpty()) {
            AeviouKeyboard.ENGLISH_BIGRAM_HINTS[lastWord]?.forEach { (tok, disp) ->
                val init = tok.take(1)
                if (init.isNotEmpty() && init !in hints) {
                    hints[init] = Triple(tok, disp, hints.size)
                }
            }
        }
        return hints
    }

    private fun refreshAeviouContextHints() {
        val hintEpoch = ++aeviouHintEpoch
        val view = aeviouView ?: return
        if (passwordEditor) {
            view.setContextHints(emptyMap())
            return
        }
        val before = readBeforeCursor(currentInputConnection, 24)
        val pred = predictionText
        val curEnv = effectiveAeviouEnv()
        if (curEnv == AeviouEnv.PURE_EN) {
            view.setContextHints(buildEnglishContextHints(before, pred))
            return
        }
        // In MIXED mode, if the preceding word is English, surface English bigram hints immediately
        val engFallback =
            if (curEnv == AeviouEnv.MIXED) buildEnglishContextHints(before, pred)
            else LinkedHashMap()
        // Instant optimistic fallback for common Chinese chars before async core response lands
        val fallback = LinkedHashMap<String, Triple<String, String, Int>>()
        var r = 0
        before.lastOrNull()?.let { ch ->
            AeviouKeyboard.CHAR_CONTEXT_HINTS[ch]?.forEach { (syl, target) ->
                val init = syl.take(1)
                if (init.isNotEmpty() && init !in fallback) fallback[init] = Triple(syl, target, r++)
            }
        }
        if (curEnv == AeviouEnv.MIXED) {
            for ((init, v) in engFallback) {
                if (init !in fallback) fallback[init] = Triple(v.first, v.second, fallback.size)
            }
        }
        if (fallback.isNotEmpty()) view.setContextHints(fallback)
        if (before.isEmpty() && pred.isEmpty()) {
            view.setContextHints(emptyMap())
            return
        }
        val gen = editor
        call(
            "context-next-hints",
            json("context_before" to before, "prediction_text" to pred),
            auxiliary = true,
        ) { resp ->
            if (!active || gen != editor || hintEpoch != aeviouHintEpoch || aeviouView !== view) return@call
            val arr = resp.optJSONArray("hints") ?: return@call
            val live = LinkedHashMap<String, Triple<String, String, Int>>()
            for (i in 0 until arr.length()) {
                val h = arr.optJSONObject(i) ?: continue
                val init = h.optString("initial")
                val syl = h.optString("syllable")
                val ch = h.optString("ch")
                if (init.isNotEmpty() && syl.isNotEmpty() && ch.isNotEmpty() && init !in live) {
                    live[init] = Triple(syl, ch, live.size)
                }
            }
            if (curEnv == AeviouEnv.MIXED) {
                for ((init, v) in engFallback) {
                    if (init !in live && live.size < 6) {
                        live[init] = Triple(v.first, v.second, live.size)
                    }
                }
            }
            if (live.isNotEmpty() || fallback.isEmpty()) {
                view.setContextHints(live)
            }
        }
    }

    private fun applyAction(r: JSONObject) {
        val ic = currentInputConnection ?: return
        val committed = if (!r.isNull("commit_text")) r.getString("commit_text") else ""
        if (committed.isNotEmpty()) {
            ic.commitText(committed, 1)
            invalidateSurroundingCache()
            if (!passwordEditor) lastCommittedText = committed
        }
        val delta = r.optInt("cursor_delta", 0)
        if (delta < 0) {
            repeat((-delta).coerceAtMost(4)) { sendDownUpKeyEvents(KeyEvent.KEYCODE_DPAD_LEFT) }
            invalidateSurroundingCache()
        }
        val preObj = r.optJSONObject("preedit")
        val pre = preObj?.optString("text").orEmpty()
        preeditText = pre
        preeditCursorUtf8 =
            if (pre.isNotEmpty()) {
                preObj?.optInt("cursor_utf8", pre.toByteArray(Charsets.UTF_8).size)
                    ?: pre.toByteArray(Charsets.UTF_8).size
            } else {
                0
            }
        shown = r.optJSONArray("candidates") ?: JSONArray()
        if (pre.isNotEmpty()) ic.setComposingText(displayPreedit(), 1)
        else if (composing && committed.isEmpty()) ic.setComposingText("", 1)
        composing = pre.isNotEmpty()
        if (!composing && syllableSplit) {
            syllableSplit = false
            Core.call(this, "set-syllable-split", json("enabled" to false))
        }
        var needsRender = false
        if (!composing && mode == "candidates") {
            mode = "keys"
            needsRender = true
        }
        if (composing) cancelPrediction()
        else if (committed.isNotEmpty() && !passwordEditor) schedulePrediction("live")
        val previousEnglish = english
        english = fieldForceEnglish || r.optString("lang_mode") == "en"
        if (!english && shiftState != 0) shiftState = 0
        else if (english && !composing) updateAutoCap(redraw = false)
        hasPreviousPage = r.optJSONObject("page")?.optBoolean("has_prev") ?: false
        hasNextPage = r.optJSONObject("page")?.optBoolean("has_next") ?: false
        if (needsRender || english != previousEnglish) {
            render()
        } else {
            refreshKeyboardCase()
            candidates()
        }
        if (!composing && committed.isNotEmpty()) {
            if (!syncAutoAeviouEnv(triggerLangSync = true)) {
                refreshAeviouContextHints()
            }
        } else {
            refreshAeviouContextHints()
        }
        when (r.optString("function_action")) {
            "settings" -> openSettingsTab(0)
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
                    .onSuccess {
                        if (it.isNotEmpty()) {
                            currentInputConnection?.commitText(it, 1)
                            invalidateSurroundingCache()
                            schedulePrediction("live")
                        }
                    }
                    .onFailure { message(it.message.orEmpty()) }
        }
    }

    companion object {
        private const val MOD_LANG_TOGGLE = 1 shl 30
    }
}
