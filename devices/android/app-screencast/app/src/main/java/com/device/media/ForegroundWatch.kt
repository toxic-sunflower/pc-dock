package com.device.media

import android.Manifest
import android.app.AppOpsManager
import android.app.usage.UsageEvents
import android.app.usage.UsageStatsManager
import android.content.Context
import android.content.pm.PackageManager
import android.os.Build
import android.os.Process
import android.util.Log

/**
 * Какое приложение сейчас на переднем плане — для хоста (STATUS 16, PROTOCOL.md §5).
 * Хост (например, исполнитель сценариев) по нему проверяет, что ввод не уходит в чужие приложения.
 *
 * Источник — [UsageStatsManager]: последнее событие «активность вышла на передний план». Нужно разрешение
 * «Доступ к истории использования» (PACKAGE_USAGE_STATS — его даёт только пользователь в настройках, один раз);
 * без него [granted] = false и пакет неизвестен. Ничего, кроме имени пакета, не читается и не хранится.
 * Не потокобезопасен: вызывать из одного потока (поток захвата сервиса).
 */
class ForegroundWatch(private val context: Context) {
    private val tag = "DeviceMedia"
    private val usm = context.getSystemService(Context.USAGE_STATS_SERVICE) as UsageStatsManager?
    private var queriedUntil = 0L

    /** Последний известный пакет на переднем плане или null. */
    var current: String? = null
        private set

    fun granted(): Boolean = hasUsageAccess(context)

    /** Дочитать события с прошлого опроса; вернуть пакет на переднем плане (или null — неизвестно). */
    fun poll(): String? {
        if (usm == null || !granted()) {
            current = null
            queriedUntil = 0
            return null
        }
        val now = System.currentTimeMillis()
        // Первый опрос — за несколько часов (приложение могло открыться давно), дальше — с небольшим перекрытием
        val begin = if (queriedUntil == 0L) now - FIRST_WINDOW_MS else queriedUntil - OVERLAP_MS
        val events = try {
            usm.queryEvents(begin, now)
        } catch (e: Exception) {
            Log.w(tag, "queryEvents: $e")
            null
        } ?: return current
        val e = UsageEvents.Event()
        var bestTs = Long.MIN_VALUE
        var best: String? = null
        while (events.hasNextEvent()) {
            events.getNextEvent(e)
            if (e.eventType == RESUMED && e.timeStamp >= bestTs) {
                bestTs = e.timeStamp
                best = e.packageName
            }
        }
        if (best != null) current = best
        queriedUntil = now
        return current
    }

    companion object {
        private const val FIRST_WINDOW_MS = 6 * 3600 * 1000L
        private const val OVERLAP_MS = 2000L

        /** MOVE_TO_FOREGROUND (API 21) = ACTIVITY_RESUMED (API 29), значение 1. */
        @Suppress("DEPRECATION")
        private const val RESUMED = UsageEvents.Event.MOVE_TO_FOREGROUND

        /** Есть ли «Доступ к истории использования» у этого приложения. */
        fun hasUsageAccess(context: Context): Boolean {
            val ops = context.getSystemService(Context.APP_OPS_SERVICE) as AppOpsManager? ?: return false
            val mode = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
                ops.unsafeCheckOpNoThrow(AppOpsManager.OPSTR_GET_USAGE_STATS, Process.myUid(), context.packageName)
            } else {
                @Suppress("DEPRECATION")
                ops.checkOpNoThrow(AppOpsManager.OPSTR_GET_USAGE_STATS, Process.myUid(), context.packageName)
            }
            return if (mode == AppOpsManager.MODE_DEFAULT) {
                context.checkCallingOrSelfPermission(Manifest.permission.PACKAGE_USAGE_STATS) == PackageManager.PERMISSION_GRANTED
            } else {
                mode == AppOpsManager.MODE_ALLOWED
            }
        }
    }
}
