package com.device.media

import android.content.Context
import android.net.ConnectivityManager
import android.net.NetworkCapabilities
import android.os.Build
import android.os.SystemClock
import org.json.JSONObject
import java.io.IOException
import java.net.ConnectException
import java.net.HttpURLConnection
import java.net.NoRouteToHostException
import java.net.ProtocolException
import java.net.SocketTimeoutException
import java.net.URL
import java.net.UnknownHostException
import javax.net.ssl.SSLException

/**
 * IP-адрес телефона для компьютера (IP_CHECK → STATUS 23, PROTOCOL.md «IP-адрес телефона (возможность 11)»;
 * без pc-dock — IpCheckReceiver через `adb shell am broadcast`).
 *
 * Только HTTPS GET без тела и без cookie, без переходов по редиректам, по сети по умолчанию (как любое приложение);
 * всё вместе — не дольше ~10 с. Ответ — одна строка JSON: тело (первые 512 символов; IP из него достаёт компьютер),
 * код HTTP, тип сети по умолчанию (cellular/wifi/ethernet/vpn/other/none) и шлюз маршрута по умолчанию.
 */
object IpCheck {
    const val MAX_URL = 512
    private const val MAX_BODY_CHARS = 512
    private const val BUDGET_MS = 10_000L
    private const val CONNECT_MS = 5_000

    /** Те же правила, что `pcdock_core::packet::valid_ip_check_url`: https://, ≤ 512, без пробелов/управляющих, без `user@` */
    fun validUrl(url: String): Boolean {
        if (url.length > MAX_URL || !url.startsWith("https://")) return false
        if (url.any { it.code <= 0x20 || it.code == 0x7f }) return false
        val rest = url.substring("https://".length)
        val authority = rest.takeWhile { it != '/' && it != '?' && it != '#' }
        if (authority.isEmpty() || authority.contains('@')) return false
        return try {
            URL(url).host.isNotEmpty()
        } catch (e: Exception) {
            false
        }
    }

    /** Тип сети по умолчанию сейчас */
    fun transport(context: Context): String {
        val cm = context.getSystemService(Context.CONNECTIVITY_SERVICE) as? ConnectivityManager ?: return "none"
        val net = cm.activeNetwork ?: return "none"
        val caps = cm.getNetworkCapabilities(net) ?: return "none"
        return when {
            caps.hasTransport(NetworkCapabilities.TRANSPORT_VPN) -> "vpn"
            caps.hasTransport(NetworkCapabilities.TRANSPORT_CELLULAR) -> "cellular"
            caps.hasTransport(NetworkCapabilities.TRANSPORT_WIFI) -> "wifi"
            caps.hasTransport(NetworkCapabilities.TRANSPORT_ETHERNET) -> "ethernet"
            else -> "other"
        }
    }

    /** Шлюз маршрута по умолчанию у сети по умолчанию (без «%wlan0» у IPv6); null — неизвестен */
    fun gateway(context: Context): String? = try {
        val cm = context.getSystemService(Context.CONNECTIVITY_SERVICE) as? ConnectivityManager
        val net = cm?.activeNetwork
        val lp = if (net != null) cm.getLinkProperties(net) else null
        lp?.routes?.firstOrNull { r ->
            r.isDefaultRoute && r.gateway != null && !r.gateway!!.isAnyLocalAddress &&
                (Build.VERSION.SDK_INT < Build.VERSION_CODES.Q || r.hasGateway())
        }?.gateway?.hostAddress?.substringBefore('%')
    } catch (e: Exception) {
        null
    }

    fun error(code: String, transport: String, ms: Long, gateway: String? = null): String {
        val o = JSONObject().put("ok", false).put("error", code).put("transport", transport).put("ms", ms)
        if (gateway != null) o.put("gateway", gateway)
        return o.toString()
    }

    private fun userAgent(context: Context): String {
        val v = try {
            context.packageManager.getPackageInfo(context.packageName, 0).versionName ?: "?"
        } catch (e: Exception) {
            "?"
        }
        return "pynelo-pcdock/$v"
    }

    /** Выполнить проверку (блокирует поток — не вызывать в потоке захвата/главном) */
    fun run(context: Context, url: String): String {
        val t0 = SystemClock.elapsedRealtime()
        val tr = transport(context)
        val gw = gateway(context)
        fun ms() = SystemClock.elapsedRealtime() - t0
        if (!validUrl(url)) return error("bad_url", tr, ms(), gw)
        if (tr == "none") return error("no_network", tr, ms(), gw)
        var conn: HttpURLConnection? = null
        return try {
            conn = URL(url).openConnection() as HttpURLConnection
            conn.requestMethod = "GET"
            conn.instanceFollowRedirects = false
            conn.useCaches = false
            conn.doOutput = false
            conn.connectTimeout = CONNECT_MS
            conn.readTimeout = CONNECT_MS
            conn.setRequestProperty("Cache-Control", "no-cache")
            conn.setRequestProperty("User-Agent", userAgent(context))
            conn.setRequestProperty("Accept", "application/json, text/plain")
            conn.connect()
            // Чтение — в остаток бюджета (всего не дольше ~10 с)
            conn.readTimeout = (BUDGET_MS - ms()).coerceIn(500L, BUDGET_MS).toInt()
            val code = conn.responseCode
            val stream = if (code >= 400) conn.errorStream else conn.inputStream
            val bytes = java.io.ByteArrayOutputStream()
            if (stream != null) {
                stream.use { s ->
                    val buf = ByteArray(1024)
                    while (bytes.size() < MAX_BODY_CHARS * 4) {
                        if (ms() > BUDGET_MS) throw SocketTimeoutException("budget")
                        val n = s.read(buf)
                        if (n < 0) break
                        bytes.write(buf, 0, n)
                    }
                }
            }
            val body = bytes.toByteArray().toString(Charsets.UTF_8).take(MAX_BODY_CHARS)
            val o = JSONObject().put("ok", true).put("code", code).put("body", body).put("transport", tr).put("ms", ms())
            if (gw != null) o.put("gateway", gw)
            o.toString()
        } catch (e: SocketTimeoutException) {
            error("timeout", tr, ms(), gw)
        } catch (e: SSLException) {
            error("ssl", tr, ms(), gw)
        } catch (e: UnknownHostException) {
            error("no_network", tr, ms(), gw)
        } catch (e: ConnectException) {
            error("no_network", tr, ms(), gw)
        } catch (e: NoRouteToHostException) {
            error("no_network", tr, ms(), gw)
        } catch (e: ProtocolException) {
            error("http", tr, ms(), gw)
        } catch (e: IOException) {
            error("io: " + e.javaClass.simpleName, tr, ms(), gw)
        } catch (e: Exception) {
            error("io: " + e.javaClass.simpleName, tr, ms(), gw)
        } finally {
            conn?.disconnect()
        }
    }
}
