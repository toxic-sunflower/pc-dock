package com.device.media

import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.view.accessibility.AccessibilityNodeInfo
import org.json.JSONObject
import java.nio.ByteBuffer
import java.nio.charset.CodingErrorAction
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicReference

/**
 * Ввод текста любого языка (кириллица, эмодзи, переводы строк) в поле, где стоит курсор (TEXT_INPUT → STATUS 25,
 * PROTOCOL.md «Ввод текста (возможность 13)»). HID-клавиатура AOA набирает только то, что есть на английской раскладке,
 * поэтому текст идёт пакетом и вставляется на телефоне.
 *
 * Порядок (первый сработавший):
 * 1. текст — в буфер обмена (ClipboardManager в главном потоке; запись из фона Android 10+ разрешает, ограничено только
 *    чтение), служба специальных возможностей [UiTreeService] находит поле с фокусом ввода и делает «Вставить»
 *    (ACTION_PASTE) — как пользователь: вставка в место курсора, приложение видит обычную вставку;
 * 2. «Задать текст» (ACTION_SET_TEXT) тем же полем: текст поля с нашим текстом на месте выделения/курсора, курсор —
 *    после вставленного (если «Вставить» не сработало или текст поля не изменился);
 * 3. службы нет, поля с фокусом нет или поле не приняло действия — текст остаётся в буфере обмена, ответ
 *    `"method":"clipboard"`: Ctrl+V жмёт хост своей HID-клавиатурой.
 *
 * Текст нигде не сохраняется и в журнал не пишется.
 */
object TextInput {
    /** Те же правила, что `pcdock_core::packet::valid_text_input`: не пустой, ≤ 16 КБ UTF-8, без U+0000. */
    const val MAX_BYTES = 16 * 1024
    /** Сколько ждать, что текст поля изменился после «Вставить» (приложение может обновить поле не сразу). */
    private const val PASTE_SETTLE_MS = 600L

    private val main = Handler(Looper.getMainLooper())

    /** Текст из payload или null, если он не подходит (пустой, длиннее предела, не UTF-8, с U+0000). */
    fun decode(bytes: ByteArray): String? {
        if (bytes.isEmpty() || bytes.size > MAX_BYTES) return null
        val text = try {
            Charsets.UTF_8.newDecoder()
                .onMalformedInput(CodingErrorAction.REPORT)
                .onUnmappableCharacter(CodingErrorAction.REPORT)
                .decode(ByteBuffer.wrap(bytes))
                .toString()
        } catch (e: Exception) {
            return null
        }
        return if (text.indexOf('\u0000') >= 0) null else text
    }

    fun error(code: String, ms: Long): String =
        JSONObject().put("ok", false).put("error", code).put("ms", ms).toString()

    private fun ok(method: String, ms: Long, reason: String? = null): String {
        val j = JSONObject().put("ok", true).put("method", method)
        if (reason != null) j.put("reason", reason)
        return j.put("ms", ms).toString()
    }

    /** Ответ на TEXT_INPUT (JSON одной строкой). Вызывать не в главном потоке и не в потоке захвата. */
    fun run(context: Context, bytes: ByteArray): String {
        val t0 = SystemClock.elapsedRealtime()
        fun ms() = SystemClock.elapsedRealtime() - t0
        val text = decode(bytes) ?: return error("bad_request", ms())

        val clipError = setClipboard(context, text)
        val svc = UiTreeService.instance
        val reason = if (svc == null) {
            "no_service"
        } else {
            val node = try {
                svc.findFocus(AccessibilityNodeInfo.FOCUS_INPUT)
                    ?: svc.rootInActiveWindow?.findFocus(AccessibilityNodeInfo.FOCUS_INPUT)
            } catch (e: Throwable) {
                null
            }
            when {
                node == null -> "no_focus"
                !node.isEditable -> "not_editable"
                else -> {
                    val before = fieldText(node)
                    if (clipError == null && paste(node, before)) return ok("paste", ms())
                    if (setText(node, before, text)) return ok("set_text", ms())
                    "action_failed"
                }
            }
        }
        if (clipError != null) return error("clipboard: $clipError", ms())
        return ok("clipboard", ms(), reason)
    }

    /** Текст поля для сравнения; null — пароль (текст недоступен). Подсказка (hint) — пустое поле. */
    private fun fieldText(n: AccessibilityNodeInfo): String? = when {
        n.isPassword -> null
        n.isShowingHintText -> ""
        else -> n.text?.toString() ?: ""
    }

    /** «Вставить» из буфера обмена; true — текст поля изменился (у пароля — поле приняло действие). */
    private fun paste(n: AccessibilityNodeInfo, before: String?): Boolean {
        val done = try {
            n.performAction(AccessibilityNodeInfo.ACTION_PASTE)
        } catch (e: Throwable) {
            false
        }
        if (!done) return false
        if (before == null) return true
        val end = SystemClock.elapsedRealtime() + PASTE_SETTLE_MS
        while (true) {
            val changed = try {
                n.refresh() && fieldText(n) != before
            } catch (e: Throwable) {
                false
            }
            if (changed) return true
            if (SystemClock.elapsedRealtime() >= end) return false
            SystemClock.sleep(50)
        }
    }

    /** «Задать текст»: прежний текст поля с [text] на месте выделения (или в конце), курсор — после вставленного. */
    private fun setText(n: AccessibilityNodeInfo, before: String?, text: String): Boolean {
        val cur = before ?: ""
        var s = n.textSelectionStart
        var e = n.textSelectionEnd
        if (s < 0 || e < 0 || s > cur.length || e > cur.length) {
            s = cur.length
            e = cur.length
        }
        if (s > e) s = e.also { e = s }
        val next = cur.substring(0, s) + text + cur.substring(e)
        val args = Bundle()
        args.putCharSequence(AccessibilityNodeInfo.ACTION_ARGUMENT_SET_TEXT_CHARSEQUENCE, next)
        val done = try {
            n.performAction(AccessibilityNodeInfo.ACTION_SET_TEXT, args)
        } catch (e: Throwable) {
            false
        }
        if (!done) return false
        try {
            val sel = Bundle()
            sel.putInt(AccessibilityNodeInfo.ACTION_ARGUMENT_SELECTION_START_INT, s + text.length)
            sel.putInt(AccessibilityNodeInfo.ACTION_ARGUMENT_SELECTION_END_INT, s + text.length)
            n.performAction(AccessibilityNodeInfo.ACTION_SET_SELECTION, sel)
        } catch (e: Throwable) {
            // курсор — не обязательно
        }
        return true
    }

    /** Кладёт текст в буфер обмена в главном потоке; null — получилось, иначе причина (класс исключения, timeout). */
    private fun setClipboard(context: Context, text: String): String? {
        val result = AtomicReference<String?>("timeout")
        val done = CountDownLatch(1)
        main.post {
            result.set(
                try {
                    val cm = context.getSystemService(ClipboardManager::class.java)
                    if (cm == null) {
                        "no_clipboard"
                    } else {
                        cm.setPrimaryClip(ClipData.newPlainText("pynelo", text))
                        null
                    }
                } catch (e: Throwable) {
                    e.javaClass.simpleName
                },
            )
            done.countDown()
        }
        if (!done.await(3, TimeUnit.SECONDS)) return "timeout"
        return result.get()
    }
}
