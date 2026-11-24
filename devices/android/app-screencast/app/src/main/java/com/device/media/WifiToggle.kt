package com.device.media

import android.accessibilityservice.AccessibilityService
import android.content.Context
import android.net.wifi.WifiManager
import android.os.Build
import android.os.SystemClock
import android.view.accessibility.AccessibilityNodeInfo
import org.json.JSONObject

/**
 * Включить или выключить Wi-Fi телефона по команде хоста (WIFI_SET → STATUS 26, PROTOCOL.md «Wi-Fi (возможность 14)»).
 * Нужно, когда у хоста нет adb (`svc wifi`): чистый режим pc-dock по USB.
 *
 * Порядок (первый сработавший):
 * 1. Wi-Fi уже в нужном состоянии — `"method":"already"`;
 * 2. `WifiManager.setWifiEnabled` — работает до Android 10 (с targetSdk 29+ на Android 10+ вызов возвращает false);
 * 3. служба специальных возможностей [UiTreeService]: если плитка Wi-Fi уже видна (хост открыл шторку жестом) — сразу
 *    к ней; иначе открыть быстрые настройки (GLOBAL_ACTION_QUICK_SETTINGS), найти
 *    плитку Wi-Fi (Wi-Fi / WLAN / «Интернет» Android 12+; MIUI — пункт центра управления) по тексту и описанию, нажать её,
 *    если состояние не то; плитка «Интернет» открывает окно со своим переключателем Wi-Fi — нажимаем его; закрыть шторку.
 *    Xiaomi/MIUI с «Центром управления»: глобальное действие открывает левую шторку уведомлений, плитки там нет —
 *    "tile_not_found" за ~1,5 с; хост открывает правую шторку жестом от правого верхнего угла и повторяет запрос;
 * 4. ответ — по факту `WifiManager.isWifiEnabled` (ждём до [CONFIRM_MS]).
 */
object WifiToggle {
    private const val CONFIRM_MS = 10_000L
    private const val TILE_WAIT_MS = 3_000L
    /** Xiaomi/MIUI: «Центр управления» — плитки в другой шторке; ждём недолго, дальше хост откроет её жестом */
    private const val XIAOMI_TILE_WAIT_MS = 1_200L
    private const val CLICK_SETTLE_MS = 3_000L

    fun error(code: String, ms: Long, enabled: Boolean? = null): String {
        val j = JSONObject().put("ok", false).put("error", code)
        if (enabled != null) j.put("enabled", enabled)
        return j.put("ms", ms).toString()
    }

    private fun ok(enabled: Boolean, method: String, ms: Long): String =
        JSONObject().put("ok", true).put("enabled", enabled).put("method", method).put("ms", ms).toString()

    /** Ответ на WIFI_SET (JSON одной строкой). Вызывать не в главном потоке и не в потоке захвата. */
    fun run(context: Context, on: Boolean): String {
        val t0 = SystemClock.elapsedRealtime()
        fun ms() = SystemClock.elapsedRealtime() - t0
        val wm = context.applicationContext.getSystemService(Context.WIFI_SERVICE) as? WifiManager
            ?: return error("no_wifi_manager", ms())
        if (wm.isWifiEnabled == on && settled(wm)) return ok(on, "already", ms())

        @Suppress("DEPRECATION")
        val apiAccepted = try {
            wm.setWifiEnabled(on)
        } catch (e: Throwable) {
            false
        }
        if (apiAccepted && waitState(wm, on, CONFIRM_MS)) return ok(on, "api", ms())

        val svc = UiTreeService.instance ?: return error("no_service", ms(), wm.isWifiEnabled)
        val r = viaQuickSettings(svc, wm, on)
        close(svc)
        if (r != null) return error(r, ms(), wm.isWifiEnabled)
        return if (waitState(wm, on, CONFIRM_MS)) ok(on, "quick_settings", ms()) else error("timeout", ms(), wm.isWifiEnabled)
    }

    private fun isXiaomi(): Boolean {
        val m = (Build.MANUFACTURER ?: "").lowercase()
        val b = (Build.BRAND ?: "").lowercase()
        return listOf("xiaomi", "redmi", "poco").any { m.contains(it) || b.contains(it) }
    }

    private fun settled(wm: WifiManager): Boolean =
        wm.wifiState == WifiManager.WIFI_STATE_ENABLED || wm.wifiState == WifiManager.WIFI_STATE_DISABLED

    private fun waitState(wm: WifiManager, on: Boolean, timeout: Long): Boolean {
        val end = SystemClock.elapsedRealtime() + timeout
        while (true) {
            val st = wm.wifiState
            if (on && st == WifiManager.WIFI_STATE_ENABLED) return true
            if (!on && st == WifiManager.WIFI_STATE_DISABLED) return true
            if (SystemClock.elapsedRealtime() >= end) return false
            SystemClock.sleep(150)
        }
    }

    /** Через шторку быстрых настроек. null — нажали (итог проверит [run]), иначе код ошибки. */
    private fun viaQuickSettings(svc: AccessibilityService, wm: WifiManager, on: Boolean): String? {
        // Шторка уже открыта хостом (MIUI: жест от правого верхнего угла после "tile_not_found") — не трогаем её:
        // GLOBAL_ACTION_QUICK_SETTINGS на MIUI с «Центром управления» открыл бы левую шторку уведомлений
        // Только окно системной шторки: «Wi-Fi» на экране приложения (например, в Настройках) — не плитка
        var tile: AccessibilityNodeInfo? = findTile(svc, shadeOnly = true)
        if (tile == null) {
            if (!svc.performGlobalAction(AccessibilityService.GLOBAL_ACTION_QUICK_SETTINGS)) return "quick_settings_failed"
            // MIUI: плитки нет в левой шторке — ответить быстро, хост откроет правую шторку жестом и спросит снова
            val end = SystemClock.elapsedRealtime() + if (isXiaomi()) XIAOMI_TILE_WAIT_MS else TILE_WAIT_MS
            while (tile == null) {
                SystemClock.sleep(300)
                tile = findTile(svc)
                if (tile == null && SystemClock.elapsedRealtime() >= end) return "tile_not_found"
            }
        }
        val found = tile ?: return "tile_not_found"
        val state = tileState(found)
        // Состояние плитки известно и уже нужное, а Wi-Fi нет — плитка врёт или ещё обновляется: ждём
        if (state == on) return if (waitState(wm, on, CLICK_SETTLE_MS)) null else "tile_state_mismatch"
        if (!click(found)) return "click_failed"
        if (waitState(wm, on, CLICK_SETTLE_MS)) return null
        // Плитка «Интернет» (Android 12+) открывает окно со своим переключателем Wi-Fi
        val sw = findDialogSwitch(svc) ?: return null
        if (sw.isCheckable && sw.isChecked == on) return null
        click(sw)
        return null
    }

    /** Закрыть шторку: Android 12+ — своим действием, раньше — «Назад» дважды (быстрые настройки → уведомления → нет). */
    private fun close(svc: AccessibilityService) {
        try {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
                svc.performGlobalAction(AccessibilityService.GLOBAL_ACTION_DISMISS_NOTIFICATION_SHADE)
            } else {
                svc.performGlobalAction(AccessibilityService.GLOBAL_ACTION_BACK)
                SystemClock.sleep(250)
                svc.performGlobalAction(AccessibilityService.GLOBAL_ACTION_BACK)
            }
        } catch (e: Throwable) {
            // шторка останется открытой — не страшно
        }
    }

    /** «Wi‑Fi» с неразрывным дефисом, «WLAN», «вай-фай» → одно написание: строчные, без пробелов и дефисов. */
    internal fun norm(s: CharSequence?): String =
        s?.toString()?.lowercase()?.filter { it.isLetterOrDigit() || it == ',' } ?: ""

    /** Насколько надпись похожа на плитку Wi-Fi: 3 — ровно «Wi-Fi»/«WLAN», 2 — начинается с них, 1 — «Интернет». */
    internal fun score(label: String): Int {
        if (label.isEmpty()) return 0
        val names = listOf("wifi", "wlan", "вайфай")
        val head = label.substringBefore(',')
        if (head in names) return 3
        if (names.any { label.startsWith(it) }) return 2
        if (head in listOf("internet", "интернет", "інтернет")) return 1
        return 0
    }

    private fun roots(svc: AccessibilityService): List<AccessibilityNodeInfo> {
        val out = ArrayList<AccessibilityNodeInfo>()
        try {
            for (w in svc.windows) w.root?.let { out.add(it) }
        } catch (e: Throwable) {
            // нет flagRetrieveInteractiveWindows — только активное окно
        }
        if (out.isEmpty()) svc.rootInActiveWindow?.let { out.add(it) }
        return out
    }

    private fun walk(n: AccessibilityNodeInfo, depth: Int, f: (AccessibilityNodeInfo) -> Unit) {
        if (depth > 40) return
        f(n)
        for (i in 0 until n.childCount) {
            val c = try { n.getChild(i) } catch (e: Throwable) { null } ?: continue
            walk(c, depth + 1, f)
        }
    }

    private fun findTile(svc: AccessibilityService, shadeOnly: Boolean = false): AccessibilityNodeInfo? {
        var best: AccessibilityNodeInfo? = null
        var bestScore = 0
        for (root in roots(svc)) {
            if (shadeOnly && root.packageName?.toString() != "com.android.systemui") continue
            walk(root, 0) { n ->
                if (!n.isVisibleToUser) return@walk
                val s = maxOf(score(norm(n.contentDescription)), score(norm(n.text)))
                if (s > bestScore) {
                    val target = clickable(n)
                    if (target != null) {
                        best = target
                        bestScore = s
                    }
                }
            }
        }
        return best
    }

    /** Сам узел или ближайший нажимаемый предок (у плитки надпись — дочерний TextView). */
    private fun clickable(n: AccessibilityNodeInfo): AccessibilityNodeInfo? {
        var cur: AccessibilityNodeInfo? = n
        var depth = 0
        while (cur != null && depth < 4) {
            if (cur.isClickable) return cur
            cur = cur.parent
            depth++
        }
        return null
    }

    /** Состояние плитки: checked / stateDescription / описание «…, вкл.». null — непонятно. */
    private fun tileState(n: AccessibilityNodeInfo): Boolean? {
        if (n.isCheckable) return n.isChecked
        val texts = ArrayList<String>()
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) texts.add(norm(n.stateDescription))
        texts.add(norm(n.contentDescription))
        for (i in 0 until n.childCount) {
            val c = try { n.getChild(i) } catch (e: Throwable) { null } ?: continue
            if (c.isCheckable) return c.isChecked
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) texts.add(norm(c.stateDescription))
        }
        for (t in texts) {
            val words = t.split(',')
            if (words.any { it in OFF }) return false
            if (words.any { it in ON }) return true
        }
        return null
    }

    private val ON = setOf("on", "вкл", "включено", "включен", "увімк", "увімкнено", "ввімкнено", "увімкнуто")
    private val OFF = setOf("off", "выкл", "выключено", "выключен", "вимк", "вимкнено")

    private fun click(n: AccessibilityNodeInfo): Boolean = try {
        n.performAction(AccessibilityNodeInfo.ACTION_CLICK)
    } catch (e: Throwable) {
        false
    }

    /** Переключатель Wi-Fi в окне плитки «Интернет»: Switch рядом с надписью Wi-Fi (или единственный Switch). */
    private fun findDialogSwitch(svc: AccessibilityService): AccessibilityNodeInfo? {
        SystemClock.sleep(600)
        val switches = ArrayList<AccessibilityNodeInfo>()
        var near: AccessibilityNodeInfo? = null
        for (root in roots(svc)) {
            walk(root, 0) { n ->
                if (!n.isVisibleToUser) return@walk
                if (n.className?.toString()?.contains("Switch") == true) {
                    switches.add(n)
                    val p = n.parent
                    if (near == null && p != null) {
                        var label = false
                        for (i in 0 until p.childCount) {
                            val c = try { p.getChild(i) } catch (e: Throwable) { null } ?: continue
                            if (score(norm(c.text)) >= 2 || score(norm(c.contentDescription)) >= 2) label = true
                        }
                        if (label) near = n
                    }
                }
            }
        }
        val sw = near ?: switches.singleOrNull() ?: return null
        return if (sw.isClickable) sw else clickable(sw) ?: sw
    }
}
