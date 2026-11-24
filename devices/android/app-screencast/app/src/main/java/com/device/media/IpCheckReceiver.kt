package com.device.media

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.util.Log

/**
 * IP-адрес телефона без pc-dock — для компьютера с adb:
 *   adb shell am broadcast --include-stopped-packages -n com.device.media/.IpCheckReceiver \
 *     -a com.device.media.IP_CHECK --es url https://…
 * Защищён разрешением DUMP (в манифесте): его держит adb shell, обычные приложения — нет. Ответ — в результате
 * широковещательного сообщения (`am broadcast` печатает `Broadcast completed: result=0, data="{…}"`) и строкой
 * журнала PyneloIpCheck. JSON тот же, что у STATUS 23 (IpCheck.run).
 */
class IpCheckReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        val url = intent.getStringExtra("url") ?: ""
        val app = context.applicationContext
        val pending = goAsync()
        Thread({
            val answer = try {
                IpCheck.run(app, url)
            } catch (e: Throwable) {
                IpCheck.error("io: " + e.javaClass.simpleName, "none", 0)
            }
            Log.i("PyneloIpCheck", answer)
            try {
                pending.resultCode = 0
                pending.resultData = answer
            } finally {
                pending.finish()
            }
        }, "ip-check-adb").start()
    }
}
