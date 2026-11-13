package com.device.media

import java.nio.ByteBuffer

/**
 * Протокол PC Dock v2 (см. devices/android/PROTOCOL.md). Все числа big-endian, строки UTF-8.
 * Формат пакета: "PD", тип (1 байт), флаги (1 байт), длина (u32), данные.
 */
object Proto {
    const val VERSION = 2
    const val HEADER_LEN = 8
    const val MAX_PAYLOAD = 16 * 1024 * 1024
    const val MAGIC_P: Byte = 0x50
    const val MAGIC_D: Byte = 0x44

    const val T_HELLO = 0x01
    const val T_PING = 0x02
    const val T_PONG = 0x03
    const val T_DISPLAY_INFO = 0x10
    const val T_VIDEO_FRAME = 0x11
    const val T_AUDIO_CONFIG = 0x20
    const val T_AUDIO_FRAME = 0x21
    const val T_REQUEST_FRAME = 0x40
    const val T_SET_STREAM = 0x41
    const val T_WAKE = 0x42
    /** Открыть приложение (хост → телефон): u32 request_id, имя пакета UTF-8. Ответ — STATUS 18. */
    const val T_LAUNCH_APP = 0x43
    /** Список приложений с главным экраном (хост → телефон): u32 request_id. Ответ — STATUS 19. */
    const val T_LIST_APPS = 0x44
    /** Элементы экрана (хост → телефон): u32 request_id. Ответ — STATUS 20 частями. */
    const val T_UI_DUMP = 0x45
    /** Причины завершения процессов приложения (хост → телефон): u32 request_id, u64 since_ms, пакет. Ответ — STATUS 22. */
    const val T_EXIT_INFO = 0x46
    /** IP-адрес телефона (хост → телефон): u32 request_id, URL (https) UTF-8. Ответ — STATUS 23. */
    const val T_IP_CHECK = 0x47
    /** HTTP-запрос в локальной Wi-Fi-сети телефона (хост → телефон): u32 request_id, JSON UTF-8. Ответ — STATUS 24. */
    const val T_HTTP_LOCAL = 0x48
    /** Ввод текста в поле с курсором (хост → телефон): u32 request_id, текст UTF-8. Ответ — STATUS 25. */
    const val T_TEXT_INPUT = 0x49
    const val T_STATUS = 0x50
    const val T_ERROR = 0x7F

    const val CAP_VIDEO_JPEG = 1 shl 0
    const val CAP_AUDIO_PCM = 1 shl 2
    const val CAP_ROTATION = 1 shl 4
    /** Телефон сообщает пакет приложения на переднем плане (STATUS 16/17), если хост попросил тем же битом в HELLO. */
    const val CAP_FOREGROUND = 1 shl 6
    /** Телефон открывает приложение по команде хоста (LAUNCH_APP, ответ STATUS 18), если хост поставил тот же бит. */
    const val CAP_LAUNCH_APP = 1 shl 7
    /** Телефон отдаёт список приложений с главным экраном (LIST_APPS, ответ STATUS 19), если хост поставил тот же бит. */
    const val CAP_LIST_APPS = 1 shl 8
    /** Телефон отдаёт элементы экрана (UI_DUMP → STATUS 20) и сообщает «экран изменился» (STATUS 21), если хост поставил бит. */
    const val CAP_UI_TREE = 1 shl 9
    /** Телефон отдаёт причины завершения процессов приложения (EXIT_INFO → STATUS 22), если хост поставил бит. */
    const val CAP_EXIT_INFO = 1 shl 10
    /** Телефон делает HTTPS GET по сети по умолчанию и отвечает телом и типом сети (IP_CHECK → STATUS 23), если хост поставил бит. */
    const val CAP_IP_CHECK = 1 shl 11
    /** Телефон делает HTTP-запрос к устройству своей Wi-Fi-сети (HTTP_LOCAL → STATUS 24), если хост поставил бит. */
    const val CAP_HTTP_LOCAL = 1 shl 12
    /** Телефон вводит текст любого языка в поле с курсором (TEXT_INPUT → STATUS 25), если хост поставил бит. */
    const val CAP_TEXT_INPUT = 1 shl 13

    const val CODEC_VIDEO_JPEG = 1
    const val CODEC_AUDIO_PCM_S16LE = 1

    const val ERR_UNSUPPORTED_VERSION = 1
    const val ERR_BAD_PACKET = 2

    const val STATUS_PROJECTION_STOPPED = 1
    const val STATUS_AUDIO_UNAVAILABLE = 2
    const val STATUS_INPUT_UNSUPPORTED = 3
    /** detail = пакет приложения на переднем плане ("" — пока неизвестно). Раз в секунду и сразу при смене. */
    const val STATUS_FOREGROUND = 16
    /** Передний план узнать нельзя, detail = причина ("usage_access" — нет «Доступа к истории использования»). */
    const val STATUS_FOREGROUND_UNAVAILABLE = 17
    /** Ответ на LAUNCH_APP: detail = "<request_id> <результат>" (ok, not_installed, no_launcher_activity, blocked_bal, invalid_package, error). */
    const val STATUS_LAUNCH_RESULT = 18
    /** Ответ на LIST_APPS: detail = "<request_id> <JSON-массив [{"package","label","icon"?}]>". */
    const val STATUS_APP_LIST = 19
    /** Ответ на UI_DUMP частями: detail = "<request_id> <i>/<n> <часть>" (склеенные части — "xml …", "no_service", "no_window", "error …"). */
    const val STATUS_UI_DUMP = 20
    /** Экран изменился (не чаще 4 раз в секунду), detail = "". */
    const val STATUS_UI_CHANGED = 21
    /** Ответ на EXIT_INFO: detail = "<request_id> <JSON-массив | unsupported | no_permission | invalid_package>". */
    const val STATUS_EXIT_INFO = 22
    /** Ответ на IP_CHECK: detail = "<request_id> <JSON-объект>" (ok, code, body, transport, ms или ok=false, error). */
    const val STATUS_IP_CHECK = 23
    /** Ответ на HTTP_LOCAL: detail = "<request_id> <JSON-объект>" (ok, code, body, headers, ms или ok=false, error, ms). */
    const val STATUS_HTTP_LOCAL = 24
    /** Ответ на TEXT_INPUT: detail = "<request_id> <JSON-объект>" (ok, method, ms или ok=false, error, ms). */
    const val STATUS_TEXT_INPUT = 25
}

class Packet(val type: Int, val flags: Int, val payload: ByteArray) {
    fun encode(): ByteArray {
        val buf = ByteBuffer.allocate(Proto.HEADER_LEN + payload.size)
        buf.put(Proto.MAGIC_P).put(Proto.MAGIC_D).put(type.toByte()).put(flags.toByte())
        buf.putInt(payload.size).put(payload)
        return buf.array()
    }
}

/** Потоковый разборщик с перестройкой после мусора (PROTOCOL.md §3). Не потокобезопасен. */
class PacketReader {
    private var buf = ByteArray(1 shl 16)
    private var len = 0
    private var resync = false
    var dropped = 0L
        private set

    fun push(src: ByteArray, n: Int) {
        if (len + n > buf.size) buf = buf.copyOf(maxOf(len + n, buf.size * 2))
        System.arraycopy(src, 0, buf, len, n)
        len += n
    }

    fun clear() {
        len = 0
        resync = false
    }

    private fun consume(n: Int) {
        System.arraycopy(buf, n, buf, 0, len - n)
        len -= n
    }

    /** Отбрасывает байты, которые не удалось разобрать. */
    private fun drop(n: Int) {
        consume(n)
        dropped += n
    }

    private fun skipToCandidate() {
        resync = true
        var pos = -1
        for (i in 1 until len - 1) {
            if (buf[i] == Proto.MAGIC_P && buf[i + 1] == Proto.MAGIC_D) {
                pos = i
                break
            }
        }
        if (pos >= 0) {
            drop(pos)
        } else {
            val keep = if (buf[len - 1] == Proto.MAGIC_P) 1 else 0
            drop(len - keep)
        }
    }

    fun next(): Packet? {
        while (true) {
            if (len < Proto.HEADER_LEN) return null
            if (buf[0] != Proto.MAGIC_P || buf[1] != Proto.MAGIC_D) {
                skipToCandidate()
                continue
            }
            val payloadLen = ByteBuffer.wrap(buf, 4, 4).int
            if (payloadLen < 0 || payloadLen > Proto.MAX_PAYLOAD) {
                resync = true
                drop(1)
                continue
            }
            val total = Proto.HEADER_LEN + payloadLen
            if (len < total) return null
            if (resync) {
                if (len >= total + 2 && (buf[total] != Proto.MAGIC_P || buf[total + 1] != Proto.MAGIC_D)) {
                    drop(1)
                    continue
                }
                resync = false
            }
            val packet =
                Packet(buf[2].toInt() and 0xFF, buf[3].toInt() and 0xFF, buf.copyOfRange(Proto.HEADER_LEN, total))
            consume(total)
            return packet
        }
    }
}

class HostHello(val versionMax: Int, val versionMin: Int, val capabilities: Int, val role: Int, val name: String)

class SetStream(
    val video: Boolean,
    val audio: Boolean,
    val maxWidth: Int,
    val maxHeight: Int,
    val jpegQuality: Int,
    val maxFps: Int,
)

class LaunchApp(val requestId: Long, val packageName: String)

class ExitInfoReq(val requestId: Long, val sinceMs: Long, val packageName: String)

class IpCheckReq(val requestId: Long, val url: String)

class HttpLocalReq(val requestId: Long, val json: String)

/** [bytes] — текст как пришёл (UTF-8); проверка и декодирование — TextInput.decode. */
class TextInputReq(val requestId: Long, val bytes: ByteArray)

object Messages {
    /** LIST_APPS: u32 request_id. null — пакет повреждён. */
    fun parseListApps(payload: ByteArray): Long? = try {
        ByteBuffer.wrap(payload).int.toLong() and 0xFFFFFFFFL
    } catch (e: Exception) {
        null
    }

    /** UI_DUMP: u32 request_id. null — пакет повреждён. */
    fun parseUiDump(payload: ByteArray): Long? = parseListApps(payload)

    /** EXIT_INFO: u32 request_id, u64 since_ms, затем имя пакета UTF-8 до конца. null — пакет повреждён. */
    fun parseExitInfo(payload: ByteArray): ExitInfoReq? = try {
        val b = ByteBuffer.wrap(payload)
        val id = b.int.toLong() and 0xFFFFFFFFL
        val since = b.long
        val name = ByteArray(b.remaining()).also { b.get(it) }.toString(Charsets.UTF_8)
        ExitInfoReq(id, since, name)
    } catch (e: Exception) {
        null
    }

    /** IP_CHECK: u32 request_id, затем URL UTF-8 до конца. null — пакет повреждён. */
    fun parseIpCheck(payload: ByteArray): IpCheckReq? = try {
        val b = ByteBuffer.wrap(payload)
        val id = b.int.toLong() and 0xFFFFFFFFL
        val url = ByteArray(b.remaining()).also { b.get(it) }.toString(Charsets.UTF_8)
        IpCheckReq(id, url)
    } catch (e: Exception) {
        null
    }

    /** HTTP_LOCAL: u32 request_id, затем JSON запроса UTF-8 до конца. null — пакет повреждён. */
    fun parseHttpLocal(payload: ByteArray): HttpLocalReq? = try {
        val b = ByteBuffer.wrap(payload)
        val id = b.int.toLong() and 0xFFFFFFFFL
        val json = ByteArray(b.remaining()).also { b.get(it) }.toString(Charsets.UTF_8)
        HttpLocalReq(id, json)
    } catch (e: Exception) {
        null
    }

    /** TEXT_INPUT: u32 request_id, затем текст UTF-8 до конца. null — пакет повреждён. */
    fun parseTextInput(payload: ByteArray): TextInputReq? = try {
        val b = ByteBuffer.wrap(payload)
        val id = b.int.toLong() and 0xFFFFFFFFL
        TextInputReq(id, ByteArray(b.remaining()).also { b.get(it) })
    } catch (e: Exception) {
        null
    }

    /** LAUNCH_APP: u32 request_id, затем имя пакета UTF-8 до конца. null — пакет повреждён. */
    fun parseLaunchApp(payload: ByteArray): LaunchApp? = try {
        val b = ByteBuffer.wrap(payload)
        val id = b.int.toLong() and 0xFFFFFFFFL
        val name = ByteArray(b.remaining()).also { b.get(it) }.toString(Charsets.UTF_8)
        LaunchApp(id, name)
    } catch (e: Exception) {
        null
    }

    fun hello(capabilities: Int, name: String): Packet {
        val nameBytes = name.toByteArray(Charsets.UTF_8).let { if (it.size > 255) it.copyOf(255) else it }
        val b = ByteBuffer.allocate(10 + nameBytes.size)
        b.putShort(Proto.VERSION.toShort()).putShort(Proto.VERSION.toShort()).putInt(capabilities)
        b.put(1) // роль: телефон
        b.put(nameBytes.size.toByte()).put(nameBytes)
        return Packet(Proto.T_HELLO, 0, b.array())
    }

    fun parseHello(payload: ByteArray): HostHello? = try {
        val b = ByteBuffer.wrap(payload)
        val vMax = b.short.toInt() and 0xFFFF
        val vMin = b.short.toInt() and 0xFFFF
        val caps = b.int
        val role = b.get().toInt()
        val n = b.get().toInt() and 0xFF
        val name = ByteArray(n).also { b.get(it) }.toString(Charsets.UTF_8)
        HostHello(vMax, vMin, caps, role, name)
    } catch (e: Exception) {
        null
    }

    fun displayInfo(streamW: Int, streamH: Int, screenW: Int, screenH: Int, dpi: Int, rotation: Int): Packet {
        val b = ByteBuffer.allocate(12)
        for (v in intArrayOf(streamW, streamH, screenW, screenH, dpi)) b.putShort(v.toShort())
        b.put(rotation.toByte()).put(0)
        return Packet(Proto.T_DISPLAY_INFO, 0, b.array())
    }

    fun videoFrame(jpeg: ByteArray, ptsUs: Long): Packet {
        val b = ByteBuffer.allocate(9 + jpeg.size)
        b.put(Proto.CODEC_VIDEO_JPEG.toByte()).putLong(ptsUs).put(jpeg)
        return Packet(Proto.T_VIDEO_FRAME, 1, b.array())
    }

    fun pong(nonce: ByteArray): Packet = Packet(Proto.T_PONG, 0, nonce)

    fun audioConfig(sampleRate: Int, channels: Int): Packet {
        val b = ByteBuffer.allocate(6)
        b.put(Proto.CODEC_AUDIO_PCM_S16LE.toByte()).putInt(sampleRate).put(channels.toByte())
        return Packet(Proto.T_AUDIO_CONFIG, 0, b.array())
    }

    fun audioFrame(pcm: ByteArray, ptsUs: Long): Packet {
        val b = ByteBuffer.allocate(8 + pcm.size)
        b.putLong(ptsUs).put(pcm)
        return Packet(Proto.T_AUDIO_FRAME, 0, b.array())
    }

    fun status(code: Int, detail: String): Packet {
        val d = detail.toByteArray(Charsets.UTF_8)
        val b = ByteBuffer.allocate(2 + d.size)
        b.putShort(code.toShort()).put(d)
        return Packet(Proto.T_STATUS, 0, b.array())
    }

    fun error(code: Int, message: String): Packet {
        val d = message.toByteArray(Charsets.UTF_8)
        val b = ByteBuffer.allocate(2 + d.size)
        b.putShort(code.toShort()).put(d)
        return Packet(Proto.T_ERROR, 0, b.array())
    }

    fun parseSetStream(payload: ByteArray): SetStream? = try {
        val b = ByteBuffer.wrap(payload)
        val flags = b.get().toInt()
        SetStream(
            video = flags and 1 != 0,
            audio = flags and 2 != 0,
            maxWidth = b.short.toInt() and 0xFFFF,
            maxHeight = b.short.toInt() and 0xFFFF,
            jpegQuality = b.get().toInt() and 0xFF,
            maxFps = b.get().toInt() and 0xFF,
        )
    } catch (e: Exception) {
        null
    }

}
