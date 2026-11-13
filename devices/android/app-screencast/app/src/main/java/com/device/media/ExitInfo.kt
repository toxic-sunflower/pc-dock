package com.device.media

import android.app.ActivityManager
import android.app.ApplicationExitInfo
import android.content.Context
import android.os.Build
import org.json.JSONArray
import org.json.JSONObject

/**
 * Почему завершались процессы приложения (EXIT_INFO → STATUS 22, PROTOCOL.md «Падения приложения (возможность 10)»).
 *
 * Android 11+: ActivityManager.getHistoricalProcessExitReasons. Для ЧУЖОГО пакета Android требует разрешение DUMP —
 * его может выдать только adb (`pm grant com.device.media android.permission.DUMP`, pynelo делает это сам); без него —
 * "no_permission". Передаются только причина, описание системы, время, pid и важность — без трассировок и данных.
 */
object ExitInfo {
    const val UNSUPPORTED = "unsupported"
    const val NO_PERMISSION = "no_permission"
    const val INVALID_PACKAGE = "invalid_package"
    /** Не больше записей в ответе */
    private const val MAX = 10

    fun query(context: Context, pkg: String, sinceMs: Long): String {
        if (!AppLauncher.validPackage(pkg)) return INVALID_PACKAGE
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.R) return UNSUPPORTED
        val am = context.getSystemService(Context.ACTIVITY_SERVICE) as ActivityManager
        val list = try {
            am.getHistoricalProcessExitReasons(pkg, 0, MAX)
        } catch (e: SecurityException) {
            return NO_PERMISSION
        } catch (e: IllegalArgumentException) {
            return "[]"
        }
        val out = JSONArray()
        for (e in list) {
            if (e.timestamp < sinceMs) continue
            out.put(
                JSONObject()
                    .put("reason", e.reason)
                    .put("name", reasonName(e.reason))
                    .put("description", (e.description ?: "").take(300))
                    .put("timestamp", e.timestamp)
                    .put("pid", e.pid)
                    .put("importance", e.importance)
                    .put("process", (e.processName ?: "").take(200))
            )
        }
        return out.toString()
    }

    private fun reasonName(r: Int): String = when (r) {
        ApplicationExitInfo.REASON_EXIT_SELF -> "exit_self"
        ApplicationExitInfo.REASON_SIGNALED -> "signaled"
        ApplicationExitInfo.REASON_LOW_MEMORY -> "low_memory"
        ApplicationExitInfo.REASON_CRASH -> "crash"
        ApplicationExitInfo.REASON_CRASH_NATIVE -> "crash_native"
        ApplicationExitInfo.REASON_ANR -> "anr"
        ApplicationExitInfo.REASON_INITIALIZATION_FAILURE -> "initialization_failure"
        ApplicationExitInfo.REASON_PERMISSION_CHANGE -> "permission_change"
        ApplicationExitInfo.REASON_EXCESSIVE_RESOURCE_USAGE -> "excessive_resource_usage"
        ApplicationExitInfo.REASON_USER_REQUESTED -> "user_requested"
        ApplicationExitInfo.REASON_USER_STOPPED -> "user_stopped"
        ApplicationExitInfo.REASON_DEPENDENCY_DIED -> "dependency_died"
        ApplicationExitInfo.REASON_OTHER -> "other"
        else -> "unknown"
    }
}
