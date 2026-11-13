package com.device.media

import android.accessibilityservice.AccessibilityService
import android.content.ComponentName
import android.content.Context
import android.graphics.Rect
import android.hardware.display.DisplayManager
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.provider.Settings
import android.text.TextUtils
import android.util.Log
import android.util.Xml
import android.view.Display
import android.view.accessibility.AccessibilityEvent
import android.view.accessibility.AccessibilityNodeInfo
import java.io.StringWriter

/**
 * Элементы экрана для хоста (UI_DUMP → STATUS 20, PROTOCOL.md «Элементы экрана (возможность 9)»).
 *
 * Служба специальных возможностей ЧИТАЕТ окно (canRetrieveWindowContent): жестов не делает, нажатия остаются
 * HID-устройствами AOA. Единственное действие — по команде хоста TEXT_INPUT вставить текст в поле с фокусом ввода
 * (ACTION_PASTE / ACTION_SET_TEXT, см. [TextInput]): кириллицу и эмодзи HID-клавиатура не наберёт. Включает пользователь (Настройки → Специальные возможности → Device Media) или хост с adb
 * (settings put secure enabled_accessibility_services). События — только смена окна/содержимого, чтобы сказать хосту
 * «экран изменился» (STATUS 21, не чаще 4 раз в секунду); сами события не читаются и не хранятся.
 *
 * Дамп — в том же виде, что `uiautomator dump` (XML с теми же атрибутами), чтобы хост искал элементы тем же кодом,
 * что и через adb. Текст полей-паролей не передаётся никогда.
 */
class UiTreeService : AccessibilityService() {

    override fun onServiceConnected() {
        Log.d(TAG, "Служба элементов экрана подключена")
        instance = this
    }

    override fun onUnbind(intent: android.content.Intent?): Boolean {
        Log.d(TAG, "Служба элементов экрана отключена")
        if (instance === this) instance = null
        return super.onUnbind(intent)
    }

    override fun onDestroy() {
        if (instance === this) instance = null
        super.onDestroy()
    }

    override fun onAccessibilityEvent(event: AccessibilityEvent?) {
        // Только факт изменения: содержимое события не читается
        onChange?.let { notifyChanged() }
    }

    override fun onInterrupt() {}

    companion object {
        private const val TAG = "DeviceMedia"
        /** Не больше элементов в дампе (защита от бесконечных списков) */
        const val MAX_NODES = 5000
        private const val MAX_DEPTH = 120
        /** «Экран изменился» — не чаще раза в столько мс */
        private const val CHANGE_GAP_MS = 250L

        @Volatile
        var instance: UiTreeService? = null
            private set

        /** Хост попросил «экран изменился» (ScreenCaptureService ставит при согласованной возможности 9) */
        @Volatile
        var onChange: (() -> Unit)? = null

        private val main = Handler(Looper.getMainLooper())
        @Volatile
        private var lastChangeAt = 0L
        @Volatile
        private var changePending = false

        private fun notifyChanged() {
            if (changePending) return
            val now = SystemClock.elapsedRealtime()
            val wait = (lastChangeAt + CHANGE_GAP_MS - now).coerceAtLeast(0)
            changePending = true
            main.postDelayed({
                changePending = false
                lastChangeAt = SystemClock.elapsedRealtime()
                try {
                    onChange?.invoke()
                } catch (e: Throwable) {
                    Log.w(TAG, "Сбой «экран изменился»: $e")
                }
            }, wait)
        }

        /** Служба включена в настройках (даже если система её ещё не подключила) */
        fun enabledInSettings(context: Context): Boolean {
            val me = ComponentName(context, UiTreeService::class.java)
            val list = Settings.Secure.getString(context.contentResolver, Settings.Secure.ENABLED_ACCESSIBILITY_SERVICES) ?: return false
            val split = TextUtils.SimpleStringSplitter(':')
            split.setString(list)
            for (s in split) {
                val c = ComponentName.unflattenFromString(s) ?: continue
                if (c == me) return true
            }
            return false
        }

        /** Служба работает (подключена системой) */
        fun running(): Boolean = instance != null

        /**
         * Ответ на UI_DUMP целиком (до деления на части): "xml <дамп>", "no_service", "no_window",
         * "error <причина>". Вызывать не в потоке захвата (обход дерева — межпроцессные вызовы).
         */
        fun dump(context: Context): String {
            val svc = instance ?: return "no_service"
            val root = try {
                svc.rootInActiveWindow
            } catch (e: Throwable) {
                return "error " + (e.message ?: e.javaClass.simpleName).take(200)
            } ?: return "no_window"
            return try {
                "xml " + serialize(root, rotation(context))
            } catch (e: Throwable) {
                Log.w(TAG, "UI_DUMP: $e")
                "error " + (e.message ?: e.javaClass.simpleName).take(200)
            }
        }

        private fun rotation(context: Context): Int = try {
            val dm = context.getSystemService(Context.DISPLAY_SERVICE) as DisplayManager
            dm.getDisplay(Display.DEFAULT_DISPLAY)?.rotation ?: 0
        } catch (e: Throwable) {
            0
        }

        /** Как AccessibilityNodeInfoDumper у uiautomator: те же имена атрибутов, невидимые элементы пропускаются */
        fun serialize(root: AccessibilityNodeInfo, rotation: Int): String {
            val w = StringWriter()
            val x = Xml.newSerializer()
            x.setOutput(w)
            x.startDocument("UTF-8", true)
            x.startTag("", "hierarchy")
            x.attribute("", "rotation", rotation.toString())
            val count = intArrayOf(0)
            node(x, root, 0, 0, count)
            x.endTag("", "hierarchy")
            x.endDocument()
            return w.toString()
        }

        private fun node(x: org.xmlpull.v1.XmlSerializer, n: AccessibilityNodeInfo, index: Int, depth: Int, count: IntArray) {
            count[0]++
            x.startTag("", "node")
            x.attribute("", "index", index.toString())
            // Пароль: текст поля не передаётся никогда (у uiautomator он и так скрыт точками — здесь пусто)
            x.attribute("", "text", if (n.isPassword) "" else safe(n.text))
            x.attribute("", "resource-id", safe(n.viewIdResourceName))
            x.attribute("", "class", safe(n.className))
            x.attribute("", "package", safe(n.packageName))
            x.attribute("", "content-desc", safe(n.contentDescription))
            x.attribute("", "hint", safe(n.hintText))
            x.attribute("", "checkable", n.isCheckable.toString())
            x.attribute("", "checked", n.isChecked.toString())
            x.attribute("", "clickable", n.isClickable.toString())
            x.attribute("", "enabled", n.isEnabled.toString())
            x.attribute("", "focusable", n.isFocusable.toString())
            x.attribute("", "focused", n.isFocused.toString())
            x.attribute("", "scrollable", n.isScrollable.toString())
            x.attribute("", "long-clickable", n.isLongClickable.toString())
            x.attribute("", "password", n.isPassword.toString())
            x.attribute("", "selected", n.isSelected.toString())
            val r = Rect()
            n.getBoundsInScreen(r)
            x.attribute("", "bounds", "[${r.left},${r.top}][${r.right},${r.bottom}]")
            if (depth < MAX_DEPTH) {
                for (i in 0 until n.childCount) {
                    if (count[0] >= MAX_NODES) break
                    val c = try {
                        n.getChild(i)
                    } catch (e: Throwable) {
                        null
                    } ?: continue
                    if (c.isVisibleToUser) node(x, c, i, depth + 1, count)
                }
            }
            x.endTag("", "node")
        }

        /** Строка для XML: недопустимые в XML символы (управляющие, одинокие суррогаты) → «?», как у uiautomator */
        private fun safe(cs: CharSequence?): String {
            if (cs == null) return ""
            val s = cs.toString()
            val b = StringBuilder(s.length)
            var i = 0
            while (i < s.length) {
                val ch = s[i]
                when {
                    Character.isHighSurrogate(ch) && i + 1 < s.length && Character.isLowSurrogate(s[i + 1]) -> {
                        b.append(ch).append(s[i + 1])
                        i++
                    }
                    Character.isSurrogate(ch) -> b.append('?')
                    ch == '\t' || ch == '\n' || ch == '\r' -> b.append(ch)
                    ch < ' ' || ch == '￾' || ch == '￿' -> b.append('?')
                    else -> b.append(ch)
                }
                i++
            }
            return b.toString()
        }

        /** Разделить ответ на части для STATUS 20 (не больше [max] символов в части) */
        fun chunks(answer: String, max: Int = 128 * 1024): List<String> {
            if (answer.length <= max) return listOf(answer)
            val out = ArrayList<String>()
            var i = 0
            while (i < answer.length) {
                var end = minOf(i + max, answer.length)
                // Не разрывать суррогатную пару (эмодзи)
                if (end < answer.length && Character.isHighSurrogate(answer[end - 1])) end--
                out.add(answer.substring(i, end))
                i = end
            }
            return out
        }
    }
}
