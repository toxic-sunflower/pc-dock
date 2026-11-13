package com.device.media

import android.content.Context
import android.net.ConnectivityManager
import android.net.Network
import android.net.NetworkCapabilities
import android.os.SystemClock
import org.json.JSONObject
import java.io.ByteArrayOutputStream
import java.net.HttpURLConnection
import java.net.SocketTimeoutException
import java.net.URL

/**
 * HTTP-запрос к устройству локальной Wi-Fi-сети телефона (HTTP_LOCAL → STATUS 24, PROTOCOL.md «HTTP в локальной сети
 * (возможность 12)»). Пример: телефон в Wi-Fi 4G-роутера, компьютер — нет; компьютер через телефон входит в админку
 * роутера (http://192.168.1.1) и перезагружает его.
 *
 * Только GET/POST, адрес — IPv4 частных сетей цифрами; запрос идёт через Wi-Fi-сеть (Network.openConnection), даже если
 * сеть по умолчанию — мобильная. Без редиректов и без cookie. Ответ — одна строка JSON: код HTTP (любой — успех),
 * тело (до 64 КБ), заголовки (имена строчными), время.
 */
object HttpLocal {
    const val MAX_URL = 2048
    const val MAX_BODY = 64 * 1024
    const val DEFAULT_TIMEOUT_MS = 10_000
    const val MAX_TIMEOUT_MS = 30_000
    private const val MAX_HEADERS = 32
    private const val TCHARS = "!#$%&'*+-.^_`|~"

    class Req(
        val method: String,
        val url: String,
        val headers: List<Pair<String, String>>,
        val body: ByteArray,
        val timeoutMs: Int,
    )

    /** Строго a.b.c.d (0–255, без ведущих нулей) из 10/8, 172.16/12, 192.168/16, 169.254/16 */
    fun privateIpv4(host: String): Boolean {
        val parts = host.split('.')
        if (parts.size != 4) return false
        val o = IntArray(4)
        for ((i, p) in parts.withIndex()) {
            if (p.isEmpty() || p.length > 3 || !p.all { it in '0'..'9' }) return false
            if (p.length > 1 && p[0] == '0') return false
            val v = p.toInt()
            if (v > 255) return false
            o[i] = v
        }
        return o[0] == 10 || (o[0] == 172 && o[1] in 16..31) || (o[0] == 192 && o[1] == 168) ||
            (o[0] == 169 && o[1] == 254)
    }

    /** Те же правила, что `pcdock_core::packet::valid_http_local_url` */
    fun validUrl(url: String): Boolean {
        val rest = when {
            url.startsWith("http://") -> url.substring("http://".length)
            url.startsWith("https://") -> url.substring("https://".length)
            else -> return false
        }
        if (url.toByteArray(Charsets.UTF_8).size > MAX_URL) return false
        if (url.any { it.code <= 0x20 || it.code == 0x7f }) return false
        val authority = rest.takeWhile { it != '/' && it != '?' && it != '#' }
        val colon = authority.indexOf(':')
        val host = if (colon >= 0) authority.substring(0, colon) else authority
        if (colon >= 0) {
            val port = authority.substring(colon + 1)
            if (port.isEmpty() || port.length > 5 || !port.all { it in '0'..'9' }) return false
            if (port.toInt() !in 1..65535) return false
        }
        if (!privateIpv4(host)) return false
        return try {
            URL(url).host == host
        } catch (e: Exception) {
            false
        }
    }

    private fun validHeader(name: String, value: String): Boolean {
        if (name.isEmpty() || name.length > 256) return false
        if (!name.all { (it in 'a'..'z') || (it in 'A'..'Z') || (it in '0'..'9') || TCHARS.indexOf(it) >= 0 }) return false
        if (value.toByteArray(Charsets.UTF_8).size > 4096) return false
        return value.none { (it.code < 0x20 && it != '\t') || it.code == 0x7f }
    }

    /** Разбор JSON запроса; null — запрос неверен (bad_request). URL проверяется отдельно (bad_url). */
    fun parse(json: String): Req? = try {
        val o = JSONObject(json)
        val method = o.optString("method", "GET")
        val url = o.getString("url")
        val headers = ArrayList<Pair<String, String>>()
        val h = o.optJSONObject("headers")
        if (h != null) {
            for (k in h.keys()) headers.add(k to h.getString(k))
        }
        val body = o.optString("body", "").toByteArray(Charsets.UTF_8)
        var timeout = o.optInt("timeout_ms", 0)
        if (timeout <= 0) timeout = DEFAULT_TIMEOUT_MS
        timeout = minOf(timeout, MAX_TIMEOUT_MS)
        val ok = (method == "GET" || method == "POST") &&
            (method == "POST" || body.isEmpty()) && body.size <= MAX_BODY &&
            headers.size <= MAX_HEADERS && headers.all { (k, v) -> validHeader(k, v) } &&
            headers.map { it.first.lowercase() }.toSet().size == headers.size
        if (ok) Req(method, url, headers, body, timeout) else null
    } catch (e: Exception) {
        null
    }

    /** Wi-Fi-сеть телефона: сеть по умолчанию, если она Wi-Fi, иначе любая сеть с Wi-Fi (не VPN); null — Wi-Fi нет */
    fun wifiNetwork(context: Context): Network? {
        val cm = context.getSystemService(Context.CONNECTIVITY_SERVICE) as? ConnectivityManager ?: return null
        fun isWifi(n: Network): Boolean {
            val c = cm.getNetworkCapabilities(n) ?: return false
            return c.hasTransport(NetworkCapabilities.TRANSPORT_WIFI) && !c.hasTransport(NetworkCapabilities.TRANSPORT_VPN)
        }
        cm.activeNetwork?.let { if (isWifi(it)) return it }
        @Suppress("DEPRECATION")
        return cm.allNetworks.firstOrNull { isWifi(it) }
    }

    fun error(code: String, ms: Long): String =
        JSONObject().put("ok", false).put("error", code).put("ms", ms).toString()

    private fun userAgent(context: Context): String {
        val v = try {
            context.packageManager.getPackageInfo(context.packageName, 0).versionName ?: "?"
        } catch (e: Exception) {
            "?"
        }
        return "pynelo-pcdock/$v"
    }

    /** Выполнить запрос (блокирует поток — не вызывать в потоке захвата/главном) */
    fun run(context: Context, json: String): String {
        val t0 = SystemClock.elapsedRealtime()
        fun ms() = SystemClock.elapsedRealtime() - t0
        val req = parse(json) ?: return error("bad_request", ms())
        if (!validUrl(req.url)) return error("bad_url", ms())
        val net = wifiNetwork(context) ?: return error("no_wifi", ms())
        var conn: HttpURLConnection? = null
        return try {
            conn = net.openConnection(URL(req.url)) as HttpURLConnection
            conn.requestMethod = req.method
            conn.instanceFollowRedirects = false
            conn.useCaches = false
            conn.connectTimeout = req.timeoutMs
            conn.readTimeout = req.timeoutMs
            conn.setRequestProperty("User-Agent", userAgent(context))
            for ((k, v) in req.headers) conn.setRequestProperty(k, v)
            if (req.method == "POST") {
                conn.doOutput = true
                conn.outputStream.use { it.write(req.body) }
            }
            val code = conn.responseCode
            // Чтение — в остаток срока
            conn.readTimeout = (req.timeoutMs - ms()).coerceIn(500L, req.timeoutMs.toLong()).toInt()
            val stream = try {
                if (code >= 400) conn.errorStream else conn.inputStream
            } catch (e: java.io.FileNotFoundException) {
                conn.errorStream
            }
            val bytes = ByteArrayOutputStream()
            var truncated = false
            stream?.use { s ->
                val buf = ByteArray(8192)
                while (true) {
                    if (ms() > req.timeoutMs) throw SocketTimeoutException("budget")
                    val n = s.read(buf)
                    if (n < 0) break
                    val room = MAX_BODY - bytes.size()
                    if (n > room) {
                        bytes.write(buf, 0, room)
                        truncated = true
                        break
                    }
                    bytes.write(buf, 0, n)
                }
            }
            val headers = JSONObject()
            val merged = LinkedHashMap<String, MutableList<String>>()
            for ((name, values) in conn.headerFields) {
                if (name == null) continue // строка статуса
                merged.getOrPut(name.lowercase()) { ArrayList() }.addAll(values)
            }
            for ((name, values) in merged) {
                headers.put(name, values.joinToString(if (name == "set-cookie") "\n" else ", "))
            }
            val o = JSONObject().put("ok", true).put("code", code)
                .put("body", bytes.toByteArray().toString(Charsets.UTF_8))
                .put("headers", headers)
            if (truncated) o.put("truncated", true)
            o.put("ms", ms()).toString()
        } catch (e: SocketTimeoutException) {
            error("timeout", ms())
        } catch (e: Exception) {
            error("io: " + e.javaClass.simpleName, ms())
        } finally {
            conn?.disconnect()
        }
    }
}
